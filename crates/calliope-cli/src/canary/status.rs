use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use super::checks::MEMORY_GROWTH_LIMIT_KIB_PER_HOUR;
use super::config::{PipelineConfig, Tier};
use super::record::{Incident, SECS_PER_DAY, Sample, utc_date, utc_datetime};

pub const HISTORY_DAYS: u64 = 30;
const WEEK_DAYS: u64 = 7;
const INCIDENTS_SHOWN: usize = 50;
const STATUS_JSON: &str = "status.json";
const STATUS_HTML: &str = "index.html";
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
    pub pipelines: Vec<PipelineStatus>,
    // newest first, open incidents have no end
    pub incidents: Vec<Incident>,
}

#[derive(Debug, Serialize)]
pub struct PipelineStatus {
    pub id: String,
    pub tier: Tier,
    pub up_now: Option<bool>,
    pub uptime_24h: Option<f64>,
    pub uptime_7d: Option<f64>,
    pub uptime_30d: Option<f64>,
    pub incidents_24h: usize,
    pub last_sample: Option<Sample>,
    // oldest first, one per utc day over HISTORY_DAYS
    pub daily: Vec<DailyUptime>,
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
    pipelines: &[PipelineConfig],
    samples: &[Sample],
    incidents: &[Incident],
    g2g_commit: &str,
    now: u64,
) -> Status {
    let window_end = now + 1;
    let day_ago = now.saturating_sub(SECS_PER_DAY);
    let first_day = (now / SECS_PER_DAY + 1).saturating_sub(HISTORY_DAYS);
    let history_start = first_day * SECS_PER_DAY;
    let pipeline_statuses = pipelines
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
            let daily = (first_day..=now / SECS_PER_DAY)
                .map(|day| {
                    let start = day * SECS_PER_DAY;
                    DailyUptime {
                        day: utc_date(start),
                        uptime: uptime_percent(own(), start, start + SECS_PER_DAY),
                    }
                })
                .collect();
            PipelineStatus {
                id: pipeline.id.clone(),
                tier: pipeline.tier,
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

fn tier_description(tier: Tier) -> &'static str {
    match tier {
        Tier::SelfHosted => {
            "A mediamtx server on the same host serves a looped clip, so a failure here is g2g's."
        }
        Tier::Public => "Third-party streams with no uptime promise.",
        Tier::Faulted => {
            "Self-hosted streams pushed through injected network faults, kept apart so the \
             faults do not count against the self-hosted numbers."
        }
    }
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
        "<tr><td>{}</td><td>{state}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td>\
         <td>{fps}</td><td>{latency}</td><td>{rss}</td><td>{growth}</td><td>{content}</td>\
         <td>{timestamps}</td><td>{drift}</td><td>{}</td></tr>",
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
            "</style></head><body>\n<h1>g2g canary</h1>\n",
        ),
        healthy = HEALTHY_COLOUR,
        failing = FAILING_COLOUR,
    );
    let _ = writeln!(
        html,
        "<p>g2g pipelines decoding live streams around the clock. A sample interval counts as up \
         when the pipeline decoded at least one frame in it. Generated {} from g2g commit \
         <code>{}</code>.</p>",
        utc_datetime(status.generated),
        escape_html(&status.g2g_commit)
    );
    html.push_str(
        "<p>The other columns are from the last one minute sample. Content compares each video \
         frame's digest with the decode of the looped clip: wrong frames are not in it, skipped \
         frames are missing between two that are. Timestamps counts output pts that went \
         backwards or jumped more than a second past the wall clock. A/V drift is how much later \
         audio arrives than video, relative to their pts. RSS growth is the slope over the last \
         day of one run.</p>\n",
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
    }
    html.push_str("<h2>Incidents</h2>\n");
    if status.incidents.is_empty() {
        html.push_str("<p>None in the last 30 days.</p>\n");
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
    use crate::canary::record::Cause;

    const G2G_COMMIT: &str = "a988298b";
    const SAMPLE_SECS: u64 = 60;
    const SAMPLES_PER_DAY: u64 = SECS_PER_DAY / SAMPLE_SECS;
    // 2026-10-17 00:00:00 UTC
    const TODAY: u64 = 1_792_195_200;

    fn pipeline(id: &str) -> PipelineConfig {
        PipelineConfig {
            id: id.into(),
            tier: Tier::SelfHosted,
            launch: "videotestsrc ! fakesink".into(),
            vod: false,
            video_sink: None,
            audio_sink: None,
            expect_hashes: None,
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
            &[pipeline("a"), pipeline("silent")],
            &samples,
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
            &[pipeline("a")],
            &[],
            &[yesterday.clone(), old, ongoing.clone()],
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
            &[pipeline("a")],
            &samples,
            &[incident("a", TODAY + 60, None)],
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
            &[pipeline("a"), faulted],
            &[sample("a", TODAY, 1800), checked],
            &[memory],
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
