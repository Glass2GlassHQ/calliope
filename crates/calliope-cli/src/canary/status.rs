use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use super::checks::{
    MEMORY_GROWTH_LIMIT_KIB_PER_HOUR, MEMORY_WINDOW_SECS, PTS_FORWARD_JUMP_NS, RECOVERY_TAIL_SECS,
};
use super::config::{CanaryConfig, Tier};
use super::record::{
    Incident, Recovery, SECS_PER_DAY, SECS_PER_HOUR, SECS_PER_MINUTE, Sample, utc_date,
    utc_datetime,
};
use super::recording::{
    DURATION_SLACK_SECS, IDLE_SEGMENTS, LOCATION_PLACEHOLDER, MAX_SIZE_TIME_PLACEHOLDER,
    RecordingCounts, SELF_HOSTED_MAX_PTS_GAP_SECS,
};
use super::schedule::{FaultKind, FaultSchedule, fault_schedule};

pub const HISTORY_DAYS: u64 = 30;
pub const WEEK_DAYS: u64 = 7;
const INCIDENTS_SHOWN: usize = 50;
const STATUS_JSON: &str = "status.json";
const STATUS_HTML: &str = "index.html";
const G2G_REPO: &str = "https://github.com/Glass2GlassHQ/glass2glass";
const NANOS_PER_SEC: i128 = 1_000_000_000;
const PERCENT: f64 = 100.0;
// sparkline bar colours by daily uptime percent
const HEALTHY_PERCENT: f64 = 99.9;
const DEGRADED_PERCENT: f64 = 99.0;
const HEALTHY_COLOUR: &str = "#2e9e44";
const DEGRADED_COLOUR: &str = "#d99a1c";
const FAILING_COLOUR: &str = "#d0402e";
const NO_DATA_COLOUR: &str = "#c8c8c8";
const BAR_WIDTH: u64 = 6;
const BAR_GAP: u64 = 2;
const SPARKLINE_HEIGHT: f64 = 24.0;
// a day at 0 percent still shows as a sliver
const MIN_BAR_HEIGHT: f64 = 2.0;

#[derive(Debug, Serialize)]
pub struct Status {
    pub generated: u64,
    // the g2g commit g2g-launch was built from
    pub g2g_commit: String,
    pub sample_secs: u64,
    pub stall_secs: u64,
    pub pipelines: Vec<PipelineStatus>,
    // newest first, open incidents have no end
    pub incidents: Vec<Incident>,
}

#[derive(Debug, Serialize)]
pub struct PipelineStatus {
    pub id: String,
    pub tier: Tier,
    // the g2g-launch line this row runs
    pub launch: String,
    pub note: Option<String>,
    pub up_now: Option<bool>,
    pub uptime_24h: Option<f64>,
    pub uptime_7d: Option<f64>,
    pub uptime_30d: Option<f64>,
    pub incidents_24h: usize,
    pub last_sample: Option<Sample>,
    // oldest first, one per utc day over HISTORY_DAYS
    pub daily: Vec<DailyUptime>,
    // a faulted row's recoveries, one per fault in schedule order
    pub recovery: Vec<FaultRecovery>,
    // on a row that records
    pub recording: Option<RecordingSummary>,
}

#[derive(Debug, Serialize)]
pub struct RecordingSummary {
    // closed files checked
    pub last_24h: RecordingCounts,
    pub last_7d: RecordingCounts,
}

#[derive(Debug, Serialize)]
pub struct FaultRecovery {
    pub fault: String,
    pub last_24h: RecoverySummary,
    pub last_7d: RecoverySummary,
}

#[derive(Debug, Default, Serialize)]
pub struct RecoverySummary {
    // clean windows judged
    pub windows: u64,
    // windows the row never recovered in
    pub unrecovered: u64,
    // the slowest recovery among the rest
    pub worst_secs: Option<f64>,
}

fn summarize_recoveries<'a>(recoveries: impl Iterator<Item = &'a Recovery>) -> RecoverySummary {
    recoveries.fold(RecoverySummary::default(), |summary, recovery| {
        let worst_secs = match (summary.worst_secs, recovery.recovery_secs) {
            (Some(worst), Some(secs)) => Some(worst.max(secs)),
            (worst, secs) => worst.or(secs),
        };
        RecoverySummary {
            windows: summary.windows + 1,
            unrecovered: summary.unrecovered + u64::from(recovery.recovery_secs.is_none()),
            worst_secs,
        }
    })
}

#[derive(Debug, Serialize)]
pub struct DailyUptime {
    pub day: String,
    pub uptime: Option<f64>,
}

// percent of the sample intervals in from..to that decoded a frame
fn uptime_percent<'a>(
    samples: impl Iterator<Item = &'a Sample>,
    from: u64,
    to: u64,
) -> Option<f64> {
    let (total, up) = samples
        .filter(|sample| sample.ts >= from && sample.ts < to)
        .fold((0u64, 0u64), |(total, up), sample| {
            (total + 1, up + u64::from(sample.up()))
        });
    (total > 0).then(|| up as f64 * PERCENT / total as f64)
}

fn overlaps(incident: &Incident, from: u64) -> bool {
    incident.end.is_none_or(|end| end >= from)
}

pub fn summarize(
    config: &CanaryConfig,
    samples: &[Sample],
    incidents: &[Incident],
    recoveries: &[Recovery],
    g2g_commit: &str,
    now: u64,
) -> Status {
    let schedule = fault_schedule();
    let window_end = now + 1;
    let day_ago = now.saturating_sub(SECS_PER_DAY);
    let first_day = (now / SECS_PER_DAY + 1).saturating_sub(HISTORY_DAYS);
    let history_start = first_day * SECS_PER_DAY;
    let pipeline_statuses = config
        .pipelines
        .iter()
        .map(|pipeline| {
            let own = || {
                samples
                    .iter()
                    .filter(|sample| sample.pipeline == pipeline.id)
            };
            let last_sample = own()
                .filter(|sample| sample.ts <= now)
                .max_by_key(|sample| sample.ts);
            let since = |days: u64| now.saturating_sub(days * SECS_PER_DAY);
            let recovery = if pipeline.judges_recovery() {
                schedule
                    .faults()
                    .map(|fault| {
                        let judged_since = |days: u64| {
                            let from = since(days);
                            recoveries.iter().filter(move |recovery| {
                                recovery.pipeline == pipeline.id
                                    && recovery.fault == fault.name
                                    && recovery.window_end >= from
                                    && recovery.window_end <= now
                            })
                        };
                        FaultRecovery {
                            fault: fault.name.to_string(),
                            last_24h: summarize_recoveries(judged_since(1)),
                            last_7d: summarize_recoveries(judged_since(WEEK_DAYS)),
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let daily = (first_day..=now / SECS_PER_DAY)
                .map(|day| {
                    let start = day * SECS_PER_DAY;
                    DailyUptime {
                        day: utc_date(start),
                        uptime: uptime_percent(own(), start, start + SECS_PER_DAY),
                    }
                })
                .collect();
            let recorded_since = |days: u64| {
                own()
                    .filter(|sample| sample.ts >= since(days) && sample.ts <= now)
                    .filter_map(|sample| sample.recording)
                    .fold(RecordingCounts::default(), RecordingCounts::add)
            };
            let recording = pipeline.recording_segment_secs.map(|_| RecordingSummary {
                last_24h: recorded_since(1),
                last_7d: recorded_since(WEEK_DAYS),
            });
            PipelineStatus {
                id: pipeline.id.clone(),
                tier: pipeline.tier,
                launch: pipeline.launch.clone(),
                note: pipeline.note.clone(),
                up_now: last_sample.map(Sample::up),
                uptime_24h: uptime_percent(own(), since(1), window_end),
                uptime_7d: uptime_percent(own(), since(WEEK_DAYS), window_end),
                uptime_30d: uptime_percent(own(), since(HISTORY_DAYS), window_end),
                incidents_24h: incidents
                    .iter()
                    .filter(|incident| {
                        incident.pipeline == pipeline.id && overlaps(incident, day_ago)
                    })
                    .count(),
                last_sample: last_sample.cloned(),
                daily,
                recovery,
                recording,
            }
        })
        .collect();
    let mut shown: Vec<Incident> = incidents
        .iter()
        .filter(|incident| overlaps(incident, history_start))
        .cloned()
        .collect();
    shown.sort_by_key(|incident| std::cmp::Reverse(incident.start));
    shown.truncate(INCIDENTS_SHOWN);
    Status {
        generated: now,
        g2g_commit: g2g_commit.to_string(),
        sample_secs: config.sample_secs,
        stall_secs: config.stall_secs,
        pipelines: pipeline_statuses,
        incidents: shown,
    }
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn format_percent(uptime: Option<f64>) -> String {
    uptime.map_or_else(|| "n/a".to_string(), |percent| format!("{percent:.2}%"))
}

fn bar_colour(uptime: Option<f64>) -> &'static str {
    match uptime {
        None => NO_DATA_COLOUR,
        Some(percent) if percent >= HEALTHY_PERCENT => HEALTHY_COLOUR,
        Some(percent) if percent >= DEGRADED_PERCENT => DEGRADED_COLOUR,
        Some(_) => FAILING_COLOUR,
    }
}

fn sparkline(daily: &[DailyUptime]) -> String {
    let width = daily.len() as u64 * (BAR_WIDTH + BAR_GAP);
    let mut svg = format!(
        r#"<svg width="{width}" height="{SPARKLINE_HEIGHT}" role="img" aria-label="daily uptime">"#
    );
    for (index, day) in daily.iter().enumerate() {
        let fraction = day.uptime.unwrap_or(PERCENT) / PERCENT;
        let height = (SPARKLINE_HEIGHT * fraction).max(MIN_BAR_HEIGHT);
        let _ = write!(
            svg,
            r#"<rect x="{}" y="{:.1}" width="{BAR_WIDTH}" height="{height:.1}" fill="{}"><title>{} {}</title></rect>"#,
            index as u64 * (BAR_WIDTH + BAR_GAP),
            SPARKLINE_HEIGHT - height,
            bar_colour(day.uptime),
            day.day,
            format_percent(day.uptime)
        );
    }
    svg.push_str("</svg>");
    svg
}

fn tier_description(tier: Tier) -> String {
    match tier {
        Tier::SelfHosted => {
            "Streams served on the same machine, so nothing outside it is involved and a \
             failure here is g2g's. A mediamtx server loops short test clips in H.264, HEVC, \
             AV1, VP9, AAC and Opus over RTSP, HLS, SRT, RIST and WebRTC, and a livesim2 \
             server plays live DASH. The egress rows test g2g as the sender: in each \
             -publish or -serve row g2g encodes a test pattern and sends it over that \
             protocol, and the matching -play row reads it back."
                .to_string()
        }
        Tier::Public => {
            "Test streams on the internet from Apple, Mux, Unified Streaming and a public \
             webcam. Nobody promises they stay up, so a down row here is usually the stream's \
             own outage. Their pictures are unknown, so they have no content check."
                .to_string()
        }
        Tier::Faulted => faulted_description(&fault_schedule()),
    }
}

fn faulted_description(schedule: &FaultSchedule) -> String {
    let faults: Vec<String> = schedule
        .windows
        .iter()
        .filter_map(|window| {
            let fault = window.fault?;
            Some(match fault.kind {
                FaultKind::Restart => fault.name.to_string(),
                FaultKind::Netem | FaultKind::Cap => {
                    format!("{} for {}", fault.name, duration_words(window.secs))
                }
            })
        })
        .collect();
    let (last_fault, other_faults) = faults.split_last().expect("fault schedule fault window");
    let fault_list = if other_faults.is_empty() {
        last_fault.clone()
    } else {
        format!("{} and {last_fault}", other_faults.join(", "))
    };
    format!(
        "The same looped clip as the self-hosted rows, served by a second mediamtx and a \
         gst-rtsp-server relay whose network is broken on purpose. Every {} it goes through {}, \
         each followed by clean network. A row has recovered once its frames match the clip with \
         none missing until the next fault, and the recovery table below shows how long that \
         took. A row that has not recovered when the next fault starts is restarted and opens a \
         no-recovery incident. Faults stall these rows and break their frames on purpose, so a \
         stall restarts none of them and a wrong frame opens no incident. The faults would lower \
         the self-hosted uptime, so these rows are listed apart.",
        duration_words(schedule.cycle_secs()),
        fault_list,
    )
}

fn duration_words(secs: u64) -> String {
    let (count, unit) = if secs.is_multiple_of(SECS_PER_MINUTE) {
        (secs / SECS_PER_MINUTE, "minute")
    } else {
        (secs, "second")
    };
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
}

fn recovery_cell(summary: &RecoverySummary) -> String {
    if summary.windows == 0 {
        return "n/a".to_string();
    }
    if summary.unrecovered > 0 {
        return alarm(
            format!("{} of {} missed", summary.unrecovered, summary.windows),
            true,
        );
    }
    or_na(summary.worst_secs.map(|secs| format!("{secs:.1} s")))
}

fn recovery_table(status: &Status) -> String {
    let rows: Vec<&PipelineStatus> = status
        .pipelines
        .iter()
        .filter(|pipeline| !pipeline.recovery.is_empty())
        .collect();
    let Some(first) = rows.first() else {
        return String::new();
    };
    let mut html = format!(
        "<p>Recovery: the slowest time from the end of each fault to the first frame of a run of \
         right frames that arrived on time and lasted until the next fault, over the last 24 hours \
         and the last {WEEK_DAYS} days. A frame is late when its delay, the time it \
         arrived minus its timestamp, exceeds the smallest delay in its run by more than \
         {RECOVERY_TAIL_SECS} seconds. Red counts the faults a row did not recover from.</p>\n\
         <table><tr><th>pipeline</th>"
    );
    for fault in &first.recovery {
        let _ = write!(html, "<th>{}</th>", escape_html(&fault.fault));
    }
    html.push_str("</tr>\n");
    for pipeline in rows {
        let _ = write!(html, "<tr><td>{}</td>", escape_html(&pipeline.id));
        for fault in &pipeline.recovery {
            let _ = write!(
                html,
                "<td>{} / {}</td>",
                recovery_cell(&fault.last_24h),
                recovery_cell(&fault.last_7d)
            );
        }
        html.push_str("</tr>\n");
    }
    html.push_str("</table>\n");
    html
}

fn recording_cell(counts: &RecordingCounts) -> String {
    alarm(
        format!("{} checked, {} failed", counts.checked, counts.failed),
        counts.failed > 0,
    )
}

fn recording_table(status: &Status) -> String {
    let rows: Vec<String> = status
        .pipelines
        .iter()
        .filter_map(|pipeline| {
            let recording = pipeline.recording.as_ref()?;
            Some(format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                escape_html(&pipeline.id),
                recording_cell(&recording.last_24h),
                recording_cell(&recording.last_7d)
            ))
        })
        .collect();
    if rows.is_empty() {
        return String::new();
    }
    format!(
        "<h2>Recordings</h2>\n<p>Rows that record their stream into a series of MP4 files with \
         <code>splitmuxsink</code>, which starts a new file at the first keyframe after each \
         segment length. The canary fills in <code>{}</code> and <code>{}</code> in their launch \
         lines. Each closed file is checked once and then deleted: it parses, starts with a \
         keyframe, its decode timestamps rise and no frame comes before the keyframe it follows, \
         every frame decodes, it lasts no longer than a segment, one keyframe interval and \
         {DURATION_SLACK_SECS} s, and no two frames are further apart than {SELF_HOSTED_MAX_PTS_GAP_SECS} s, \
         or {} s on a faulted row, its longest fault plus the clean window after it. A file that \
         fails is kept for inspection and opens a recording incident.</p>\n\
         <table><tr><th>pipeline</th><th>24h</th><th>{WEEK_DAYS}d</th></tr>\n{}</table>\n",
        escape_html(LOCATION_PLACEHOLDER),
        escape_html(MAX_SIZE_TIME_PLACEHOLDER),
        fault_schedule().longest_fault_and_recovery_secs(),
        rows.concat()
    )
}

fn or_na(value: Option<String>) -> String {
    value.unwrap_or_else(|| "n/a".to_string())
}

fn alarm(text: String, bad: bool) -> String {
    if bad {
        format!(r#"<span class="down">{text}</span>"#)
    } else {
        text
    }
}

fn pipeline_row(pipeline: &PipelineStatus) -> String {
    let state = match pipeline.up_now {
        Some(true) => r#"<span class="up">up</span>"#,
        Some(false) => r#"<span class="down">down</span>"#,
        None => "n/a",
    };
    let last = pipeline.last_sample.as_ref();
    let fps = or_na(last.map(|sample| format!("{:.1}", sample.fps)));
    let latency = or_na(
        last.and_then(|sample| sample.decode_latency_ms)
            .map(|ms| format!("{ms:.2} ms")),
    );
    let rss = or_na(
        last.and_then(|sample| sample.rss_kb)
            .map(|kb| format!("{} MiB", kb / 1024)),
    );
    let growth = or_na(
        last.and_then(|sample| sample.rss_growth_kib_per_hour)
            .map(|kib| {
                alarm(
                    format!("{kib:+.0} KiB/h"),
                    kib > MEMORY_GROWTH_LIMIT_KIB_PER_HOUR,
                )
            }),
    );
    let content = or_na(last.and_then(|sample| sample.content).map(|counts| {
        alarm(
            format!(
                "{} ok, {} wrong, {} skipped",
                counts.matched, counts.mismatched, counts.skipped
            ),
            counts.bad() > 0,
        )
    }));
    let timestamps = or_na(last.and_then(|sample| sample.timestamps).map(|counts| {
        alarm(
            format!("{} back, {} jumps", counts.backward, counts.forward_jumps),
            counts.bad() > 0,
        )
    }));
    let drift = or_na(
        last.and_then(|sample| sample.av_drift_ms)
            .map(|ms| format!("{ms:+.1} ms")),
    );
    format!(
        "<tr><td title=\"{}\">{}</td><td>{state}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td>\
         <td>{fps}</td><td>{latency}</td><td>{rss}</td><td>{growth}</td><td>{content}</td>\
         <td>{timestamps}</td><td>{drift}</td><td>{}</td></tr>",
        escape_html(&pipeline.launch),
        escape_html(&pipeline.id),
        format_percent(pipeline.uptime_24h),
        format_percent(pipeline.uptime_7d),
        format_percent(pipeline.uptime_30d),
        pipeline.incidents_24h,
        sparkline(&pipeline.daily)
    )
}

pub fn render_html(status: &Status) -> String {
    let mut html = format!(
        concat!(
            "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">",
            "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">",
            "<title>g2g canary</title><style>",
            "body{{font-family:system-ui,sans-serif;margin:2rem;color:#222;max-width:80rem}}",
            "table{{border-collapse:collapse;width:100%;margin-bottom:2rem}}",
            "th,td{{text-align:left;padding:.35rem .6rem;border-bottom:1px solid #ddd;vertical-align:top}}",
            "th{{font-weight:600}}.up{{color:{healthy}}}.down{{color:{failing}}}",
            "code{{font-size:.85em;word-break:break-all}}",
            "dt{{font-weight:600}}dd{{margin:0 0 .6rem 1.2rem}}",
            "</style></head><body>\n<h1>g2g canary</h1>\n",
        ),
        healthy = HEALTHY_COLOUR,
        failing = FAILING_COLOUR,
    );
    let _ = writeln!(
        html,
        "<p><a href=\"{G2G_REPO}\">g2g</a> is a media pipeline framework written in Rust. This \
         page tracks g2g pipelines that receive, decode and send live video and audio around \
         the clock on one cloud server. Each row is one pipeline run by <code>g2g-launch</code>, \
         which takes the same pipeline syntax as <code>gst-launch-1.0</code>. A supervisor \
         restarts a pipeline that exits or produces no frame for {} seconds, and samples every \
         pipeline every {} seconds.</p>\n<p>Generated {} from g2g commit \
         <a href=\"{G2G_REPO}/commit/{commit}\"><code>{commit}</code></a>, which every pipeline \
         on this page runs.</p>",
        status.stall_secs,
        status.sample_secs,
        utc_datetime(status.generated),
        commit = escape_html(&status.g2g_commit)
    );
    let _ = writeln!(
        html,
        "<h2>Reading a row</h2>\n<dl>\n\
         <dt>pipeline</dt><dd>Hover over the name to see the pipeline, or find it under \
         \"What each pipeline runs\" below.</dd>\n\
         <dt>now</dt><dd>Up when the pipeline produced at least one frame in the last \
         sample.</dd>\n\
         <dt>24h, 7d, 30d</dt><dd>The share of samples in which the pipeline produced at least \
         one frame. A restart that takes a few seconds does not lose a sample.</dd>\n\
         <dt>incidents 24h</dt><dd>Problems in the last day, each listed under Incidents at \
         the bottom. A short one does not lower the uptime.</dd>\n\
         <dt>fps</dt><dd>Frames per second in the last sample. A row with audio and video adds \
         audio buffers and video frames together.</dd>\n\
         <dt>decode p50</dt><dd>The median time the decoder took per frame in the last sample, \
         the slowest decoder on a row with several. Rows that only send show n/a.</dd>\n\
         <dt>rss</dt><dd>The memory the g2g process uses.</dd>\n\
         <dt>rss growth</dt><dd>How fast that memory grew over the last {memory_hours} hours of \
         one uninterrupted run, shown red above {memory_limit} KiB per hour, which suggests a \
         leak. It shows n/a until a run has lasted that long.</dd>\n\
         <dt>content</dt><dd>Whether the decoded video is the right picture. Each frame's \
         digest is looked up in a table of the looped clip's frames: ok frames are in it, \
         wrong frames are not, and skipped frames are missing between two ok ones. Rows whose \
         source is not the clip show n/a.</dd>\n\
         <dt>timestamps</dt><dd>Output timestamps that went backwards, or jumped more than \
         {jump_secs} s ahead of the real time that passed.</dd>\n\
         <dt>a/v drift</dt><dd>On a row with audio and video, how much later audio arrives \
         than the video frame with the same timestamp. Negative means audio arrives \
         first.</dd>\n\
         <dt>last 30 days</dt><dd>One bar per day, as tall as that day's uptime: \
         <span style=\"color:{HEALTHY_COLOUR}\">green</span> at {HEALTHY_PERCENT}% or more, \
         <span style=\"color:{DEGRADED_COLOUR}\">amber</span> at {DEGRADED_PERCENT}% or more, \
         <span style=\"color:{FAILING_COLOUR}\">red</span> below that and \
         <span style=\"color:{NO_DATA_COLOUR}\">grey</span> with no samples.</dd>\n</dl>",
        memory_hours = MEMORY_WINDOW_SECS / SECS_PER_HOUR,
        memory_limit = MEMORY_GROWTH_LIMIT_KIB_PER_HOUR,
        jump_secs = PTS_FORWARD_JUMP_NS / NANOS_PER_SEC,
    );
    for tier in Tier::ALL {
        let rows: Vec<String> = status
            .pipelines
            .iter()
            .filter(|pipeline| pipeline.tier == tier)
            .map(pipeline_row)
            .collect();
        if rows.is_empty() {
            continue;
        }
        let _ = writeln!(
            html,
            "<h2>{}</h2>\n<p>{}</p>\n<table><tr><th>pipeline</th><th>now</th><th>24h</th>\
             <th>7d</th><th>30d</th><th>incidents 24h</th><th>fps</th><th>decode p50</th>\
             <th>rss</th><th>rss growth</th><th>content</th><th>timestamps</th>\
             <th>a/v drift</th><th>last 30 days</th></tr>",
            tier.label(),
            tier_description(tier)
        );
        for row in rows {
            html.push_str(&row);
            html.push('\n');
        }
        html.push_str("</table>\n");
        if tier == Tier::Faulted {
            html.push_str(&recovery_table(status));
        }
    }
    html.push_str(&recording_table(status));
    html.push_str(
        "<h2>What each pipeline runs</h2>\n<p>The <code>g2g-launch</code> pipeline of every row \
         above. <code>checksumsink</code> digests each decoded frame for the content check, and \
         <code>fakesink</code> drops frames on rows without one.</p>\n\
         <table><tr><th>pipeline</th><th>tier</th><th>launch</th><th>note</th></tr>\n",
    );
    for pipeline in &status.pipelines {
        let _ = writeln!(
            html,
            "<tr><td>{}</td><td>{}</td><td><code>{}</code></td><td>{}</td></tr>",
            escape_html(&pipeline.id),
            pipeline.tier.label(),
            escape_html(&pipeline.launch),
            escape_html(pipeline.note.as_deref().unwrap_or_default())
        );
    }
    html.push_str("</table>\n");
    let _ = writeln!(
        html,
        "<h2>Incidents</h2>\n<p>The newest {INCIDENTS_SHOWN} problems of the last {HISTORY_DAYS} \
         days. A down incident means the pipeline exited, was killed or produced no frame for \
         {} seconds, and the supervisor restarted it. A content, timestamps or memory-growth \
         incident means the check of that name above failed. A no-recovery incident means a \
         faulted row had not recovered when the next fault started, and the supervisor \
         restarted it. A recording incident names a recorded file that failed a check, or says \
         no file closed for {IDLE_SEGMENTS} segment lengths, and closes on the next file that \
         passes. Restarts counts the restarts during the incident, and last error is the last \
         error g2g printed before it.</p>",
        status.stall_secs
    );
    if status.incidents.is_empty() {
        html.push_str("<p>None.</p>\n");
    } else {
        html.push_str(
            "<table><tr><th>pipeline</th><th>tier</th><th>kind</th><th>start</th><th>end</th>\
             <th>cause</th><th>restarts</th><th>last error</th></tr>\n",
        );
        for incident in &status.incidents {
            let _ = writeln!(
                html,
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><code>{}</code></td></tr>",
                escape_html(&incident.pipeline),
                incident.tier.label(),
                incident.cause.kind().label(),
                utc_datetime(incident.start),
                incident
                    .end
                    .map_or_else(|| "ongoing".to_string(), utc_datetime),
                escape_html(&incident.cause.to_string()),
                incident.restarts,
                escape_html(incident.last_error.as_deref().unwrap_or(""))
            );
        }
        html.push_str("</table>\n");
    }
    html.push_str("</body></html>\n");
    html
}

// write then rename, so a sync never publishes a half-written page
fn replace_file(dir: &Path, name: &str, contents: &str) -> Result<()> {
    let path = dir.join(name);
    let partial = dir.join(format!(".{name}.partial"));
    std::fs::write(&partial, contents).with_context(|| format!("write {}", partial.display()))?;
    std::fs::rename(&partial, &path).with_context(|| format!("rename to {}", path.display()))
}

pub fn write_status(dir: &Path, status: &Status) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    replace_file(dir, STATUS_JSON, &serde_json::to_string_pretty(status)?)?;
    replace_file(dir, STATUS_HTML, &render_html(status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canary::checks::{ContentCounts, TimestampCounts};
    use crate::canary::config::PipelineConfig;
    use crate::canary::record::Cause;

    const G2G_COMMIT: &str = "a988298b";
    const SAMPLE_SECS: u64 = 60;
    const STALL_SECS: u64 = 30;
    const SAMPLES_PER_DAY: u64 = SECS_PER_DAY / SAMPLE_SECS;
    // 2026-10-17 00:00:00 UTC
    const TODAY: u64 = 1_792_195_200;

    fn canary(pipelines: Vec<PipelineConfig>) -> CanaryConfig {
        CanaryConfig {
            log_dir: "log".into(),
            status_dir: "status".into(),
            g2g_launch: None,
            sample_secs: SAMPLE_SECS,
            stall_secs: STALL_SECS,
            observe_base_port: 0,
            backoff_min_secs: 1,
            backoff_max_secs: 1,
            healthy_run_secs: 1,
            startup_grace_secs: 0,
            recordings_dir: None,
            pipelines,
        }
    }

    fn pipeline(id: &str) -> PipelineConfig {
        PipelineConfig {
            id: id.into(),
            tier: Tier::SelfHosted,
            launch: "videotestsrc ! fakesink".into(),
            ends: false,
            note: None,
            video_sink: None,
            audio_sink: None,
            expect_hashes: None,
            recording_segment_secs: None,
            checked_by: None,
        }
    }

    fn sample(pipeline: &str, ts: u64, frames: u64) -> Sample {
        Sample {
            ts,
            pipeline: pipeline.into(),
            tier: Tier::SelfHosted,
            alive: frames > 0,
            frames,
            fps: frames as f64 / SAMPLE_SECS as f64,
            decode_latency_ms: None,
            rss_kb: None,
            restarts: 0,
            last_error: None,
            content: None,
            timestamps: None,
            av_drift_ms: None,
            rss_growth_kib_per_hour: None,
            recording: None,
        }
    }

    // one sample a minute over a whole day, the first `down` minutes decoding nothing
    fn day_of_samples(pipeline: &str, day_start: u64, down: u64) -> Vec<Sample> {
        (0..SAMPLES_PER_DAY)
            .map(|minute| {
                let frames = if minute < down { 0 } else { 1800 };
                sample(pipeline, day_start + minute * SAMPLE_SECS, frames)
            })
            .collect()
    }

    fn incident(pipeline: &str, start: u64, end: Option<u64>) -> Incident {
        Incident {
            id: format!("{pipeline}-{start}"),
            pipeline: pipeline.into(),
            tier: Tier::SelfHosted,
            start,
            end,
            cause: Cause::Stall { secs: 30 },
            restarts: 1,
            last_error: Some("<rtsp> connection refused".into()),
            stderr: None,
        }
    }

    #[test]
    fn uptime_windows_and_daily_buckets() {
        let yesterday = TODAY - SECS_PER_DAY;
        let eight_days_ago = TODAY - 8 * SECS_PER_DAY;
        let mut samples = day_of_samples("a", eight_days_ago, SAMPLES_PER_DAY);
        samples.extend(day_of_samples("a", yesterday, 144));
        samples.extend(day_of_samples("a", TODAY, 0));
        let now = TODAY + SECS_PER_DAY - 1;
        let status = summarize(
            &canary(vec![pipeline("a"), pipeline("silent")]),
            &samples,
            &[],
            &[],
            G2G_COMMIT,
            now,
        );

        let a = &status.pipelines[0];
        assert_eq!(a.uptime_24h, Some(100.0));
        // yesterday lost 144 of 1440 minutes, today none
        assert_eq!(a.uptime_7d, Some(95.0));
        // the down day eight days back only shows in the 30 day window
        let thirty_days = (3 * SAMPLES_PER_DAY - SAMPLES_PER_DAY - 144) as f64 * 100.0
            / (3 * SAMPLES_PER_DAY) as f64;
        assert_eq!(a.uptime_30d, Some(thirty_days));
        assert_eq!(a.up_now, Some(true));
        assert_eq!(a.daily.len(), HISTORY_DAYS as usize);
        assert_eq!(a.daily.last().unwrap().day, "2026-10-17");
        assert_eq!(a.daily[a.daily.len() - 2].uptime, Some(90.0));
        assert_eq!(a.daily[a.daily.len() - 9].uptime, Some(0.0));
        assert_eq!(a.daily[0].uptime, None);

        let silent = &status.pipelines[1];
        assert_eq!(silent.uptime_24h, None);
        assert_eq!(silent.up_now, None);
    }

    #[test]
    fn incidents_newest_first_with_open_ones_counted() {
        let old = incident(
            "a",
            TODAY - 40 * SECS_PER_DAY,
            Some(TODAY - 40 * SECS_PER_DAY + 60),
        );
        let yesterday = incident(
            "a",
            TODAY - SECS_PER_DAY / 2,
            Some(TODAY - SECS_PER_DAY / 2 + 90),
        );
        let ongoing = incident("a", TODAY + 600, None);
        let now = TODAY + 3_600;
        let status = summarize(
            &canary(vec![pipeline("a")]),
            &[],
            &[yesterday.clone(), old, ongoing.clone()],
            &[],
            G2G_COMMIT,
            now,
        );
        assert_eq!(status.incidents, vec![ongoing, yesterday]);
        assert_eq!(status.pipelines[0].incidents_24h, 2);
    }

    #[test]
    fn html_escapes_stream_text_and_marks_open_incidents() {
        let samples = day_of_samples("a", TODAY, 0);
        let status = summarize(
            &canary(vec![pipeline("a")]),
            &samples,
            &[incident("a", TODAY + 60, None)],
            &[],
            G2G_COMMIT,
            TODAY + SECS_PER_DAY - 1,
        );
        let html = render_html(&status);
        assert!(html.contains("&lt;rtsp&gt; connection refused"));
        assert!(!html.contains("<rtsp>"));
        assert!(html.contains("ongoing"));
        assert!(html.contains("stall: no frame for 30 s"));
        assert!(html.contains(HEALTHY_COLOUR));
    }

    #[test]
    fn each_row_carries_its_launch_line() {
        let mut config = pipeline("a");
        config.launch = "rtspsrc location=\"rtsp://x\" ! decodebin ! fakesink".into();
        let status = summarize(&canary(vec![config]), &[], &[], &[], G2G_COMMIT, TODAY);
        assert_eq!(
            status.pipelines[0].launch,
            "rtspsrc location=\"rtsp://x\" ! decodebin ! fakesink"
        );
        let html = render_html(&status);
        let escaped = "rtspsrc location=&quot;rtsp://x&quot; ! decodebin ! fakesink";
        assert!(html.contains(&format!("<td title=\"{escaped}\">a</td>")));
        assert!(html.contains(&format!("<td><code>{escaped}</code></td>")));
    }

    #[test]
    fn a_row_note_shows_beside_its_launch_line() {
        let mut config = pipeline("a");
        config.note = Some("ends at 03:20 & 15:20".into());
        let status = summarize(&canary(vec![config]), &[], &[], &[], G2G_COMMIT, TODAY);
        let html = render_html(&status);
        assert!(html.contains("</code></td><td>ends at 03:20 &amp; 15:20</td></tr>"));
    }

    #[test]
    fn the_faulted_description_names_each_fault_of_the_schedule() {
        let schedule = fault_schedule();
        let description = tier_description(Tier::Faulted);
        for window in &schedule.windows {
            let Some(fault) = window.fault else {
                continue;
            };
            let named = match fault.kind {
                FaultKind::Restart => fault.name.to_string(),
                FaultKind::Netem | FaultKind::Cap => {
                    format!("{} for {}", fault.name, duration_words(window.secs))
                }
            };
            assert!(description.contains(&named), "{named} in {description}");
        }
        let cycle = format!("Every {} it", duration_words(schedule.cycle_secs()));
        assert!(description.contains(&cycle), "{description}");
    }

    fn recovery(pipeline: &str, fault: &str, window_end: u64, secs: Option<f64>) -> Recovery {
        Recovery {
            pipeline: pipeline.into(),
            fault: fault.into(),
            fault_end: window_end - 60,
            window_end,
            recovery_secs: secs,
            av_drift_ms: None,
        }
    }

    #[test]
    fn faulted_rows_show_their_slowest_recovery_per_fault() {
        let schedule = fault_schedule();
        let faults: Vec<&str> = schedule.faults().map(|fault| fault.name).collect();
        let (first, second) = (faults[0], faults[1]);
        let mut faulted = pipeline("lossy-rtsp");
        faulted.tier = Tier::Faulted;
        let now = TODAY + SECS_PER_DAY - 1;
        let recoveries = [
            recovery("lossy-rtsp", first, now - 100, Some(1.5)),
            recovery("lossy-rtsp", first, now - 200, Some(4.3)),
            // three days ago, only in the week
            recovery("lossy-rtsp", first, now - 3 * SECS_PER_DAY, Some(9.0)),
            recovery("lossy-rtsp", second, now - 300, None),
            recovery("lossy-rtsp", second, now - 400, Some(2.0)),
            // another row's records stay on that row
            recovery("a", first, now - 100, None),
        ];
        let status = summarize(
            &canary(vec![pipeline("a"), faulted]),
            &[],
            &[],
            &recoveries,
            G2G_COMMIT,
            now,
        );
        assert!(status.pipelines[0].recovery.is_empty(), "not a faulted row");
        let row = &status.pipelines[1].recovery;
        assert_eq!(row.len(), faults.len(), "a column per fault");
        assert_eq!(row[0].fault, first);
        assert_eq!(row[0].last_24h.windows, 2);
        assert_eq!(row[0].last_24h.worst_secs, Some(4.3));
        assert_eq!(row[0].last_7d.worst_secs, Some(9.0));
        assert_eq!(row[1].last_24h.unrecovered, 1);
        assert_eq!(row[2].last_24h.windows, 0);

        let html = render_html(&status);
        let table = &html[html.find("Recovery:").unwrap()..];
        assert!(table.contains("4.3 s / 9.0 s"), "{table}");
        assert!(table.contains("1 of 2 missed"), "{table}");
        assert!(table.contains("n/a / n/a"), "{table}");
    }

    #[test]
    fn recording_rows_sum_their_checked_files() {
        let mut recording = pipeline("record");
        recording.recording_segment_secs = Some(60);
        let now = TODAY + SECS_PER_DAY - 1;
        let counted = |ts: u64, checked: u64, failed: u64| Sample {
            recording: Some(RecordingCounts { checked, failed }),
            ..sample("record", ts, 1800)
        };
        let samples = [
            counted(now - 100, 1, 0),
            counted(now - 40, 2, 1),
            // three days ago, only in the week
            counted(now - 3 * SECS_PER_DAY, 4, 0),
            sample("a", now - 40, 1800),
        ];
        let status = summarize(
            &canary(vec![pipeline("a"), recording]),
            &samples,
            &[],
            &[],
            G2G_COMMIT,
            now,
        );
        assert!(
            status.pipelines[0].recording.is_none(),
            "not a recording row"
        );
        let summary = status.pipelines[1].recording.as_ref().unwrap();
        assert_eq!(
            summary.last_24h,
            RecordingCounts {
                checked: 3,
                failed: 1
            }
        );
        assert_eq!(
            summary.last_7d,
            RecordingCounts {
                checked: 7,
                failed: 1
            }
        );

        let html = render_html(&status);
        let table = &html[html.find("<h2>Recordings</h2>").unwrap()..];
        assert!(
            table.contains(
                "<td>record</td><td><span class=\"down\">3 checked, 1 failed</span></td>"
            ),
            "{table}"
        );
    }

    #[test]
    fn faulted_tier_gets_its_own_group_and_checks_show() {
        let mut faulted = pipeline("lossy-rtsp");
        faulted.tier = Tier::Faulted;
        let mut checked = sample("lossy-rtsp", TODAY, 1800);
        checked.tier = Tier::Faulted;
        checked.content = Some(ContentCounts {
            matched: 1790,
            mismatched: 4,
            skipped: 6,
        });
        checked.timestamps = Some(TimestampCounts {
            frames: 1800,
            backward: 1,
            forward_jumps: 0,
        });
        checked.av_drift_ms = Some(-12.5);
        checked.rss_growth_kib_per_hour = Some(900.0);
        let memory = Incident {
            cause: Cause::MemoryGrowth {
                kib_per_hour: 900.0,
            },
            ..incident("lossy-rtsp", TODAY, None)
        };
        let status = summarize(
            &canary(vec![pipeline("a"), faulted]),
            &[sample("a", TODAY, 1800), checked],
            &[memory],
            &[],
            G2G_COMMIT,
            TODAY + 30,
        );
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["g2g_commit"], G2G_COMMIT);
        assert_eq!(json["pipelines"][1]["tier"], "faulted");
        assert_eq!(json["incidents"][0]["end"], serde_json::Value::Null);

        let html = render_html(&status);
        let self_hosted = html.find("<h2>self-hosted</h2>").unwrap();
        let faulted_group = html.find("<h2>faulted</h2>").unwrap();
        let faulted_row = html.find("<td>lossy-rtsp</td>").unwrap();
        assert!(self_hosted < faulted_group && faulted_group < faulted_row);
        assert!(
            !html.contains("<h2>public</h2>"),
            "an empty tier has no group"
        );
        assert!(html.contains(&format!("<code>{G2G_COMMIT}</code>")));
        assert!(html.contains("1790 ok, 4 wrong, 6 skipped"));
        assert!(html.contains("1 back, 0 jumps"));
        assert!(html.contains("-12.5 ms"));
        assert!(html.contains("+900 KiB/h"));
        assert!(html.contains("<td>memory-growth</td>"));
        assert!(html.contains("memory growth: rss up 900 KiB/h over a day"));
    }
}
