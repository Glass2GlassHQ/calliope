use std::fmt;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::checks::{ContentCounts, TimestampCounts};
use super::config::{PipelineConfig, Tier};

pub const SECS_PER_DAY: u64 = 86_400;
pub const SECS_PER_HOUR: u64 = 3_600;
pub const SECS_PER_MINUTE: u64 = 60;
pub const SAMPLES_PREFIX: &str = "samples";
pub const INCIDENTS_PREFIX: &str = "incidents";
pub const RECOVERIES_PREFIX: &str = "recoveries";
pub const STDERR_DIR: &str = "stderr";
const OPEN_INCIDENTS_FILE: &str = "open-incidents.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub ts: u64,
    pub pipeline: String,
    pub tier: Tier,
    // a g2g-launch process exists at sample time
    pub alive: bool,
    // frames into the sinks since the previous sample
    pub frames: u64,
    pub fps: f64,
    pub decode_latency_ms: Option<f64>,
    pub rss_kb: Option<u64>,
    // restarts since the canary started
    pub restarts: u64,
    pub last_error: Option<String>,
    // the video sink's frames against expect-hashes
    pub content: Option<ContentCounts>,
    // checksum lines from every sink, none when no line arrived
    pub timestamps: Option<TimestampCounts>,
    pub av_drift_ms: Option<f64>,
    // the run's rss slope over the last day, none until it has run a day
    pub rss_growth_kib_per_hour: Option<f64>,
}

impl Sample {
    // uptime counts a sample interval as up when it decoded at least one frame
    pub fn up(&self) -> bool {
        self.frames > 0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Cause {
    Exit { code: i32 },
    Signal { signal: i32 },
    Stall { secs: u64 },
    SpawnFailed { message: String },
    // totals over the incident
    Content { mismatched: u64, skipped: u64 },
    Timestamps { backward: u64, forward_jumps: u64 },
    // the highest slope while open
    MemoryGrowth { kib_per_hour: f64 },
    // no unbroken run of right frames lasted the clean window after this fault
    NoRecovery { fault: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncidentKind {
    Down,
    Content,
    Timestamps,
    MemoryGrowth,
    NoRecovery,
}

impl IncidentKind {
    pub fn label(self) -> &'static str {
        match self {
            IncidentKind::Down => "down",
            IncidentKind::Content => "content",
            IncidentKind::Timestamps => "timestamps",
            IncidentKind::MemoryGrowth => "memory-growth",
            IncidentKind::NoRecovery => "no-recovery",
        }
    }
}

impl Cause {
    pub fn kind(&self) -> IncidentKind {
        match self {
            Cause::Exit { .. } | Cause::Signal { .. } | Cause::Stall { .. } => IncidentKind::Down,
            Cause::SpawnFailed { .. } => IncidentKind::Down,
            Cause::Content { .. } => IncidentKind::Content,
            Cause::Timestamps { .. } => IncidentKind::Timestamps,
            Cause::MemoryGrowth { .. } => IncidentKind::MemoryGrowth,
            Cause::NoRecovery { .. } => IncidentKind::NoRecovery,
        }
    }

    // folds a later sample's finding of the same kind into an open incident
    fn absorb(&mut self, later: Cause) {
        match (self, later) {
            (
                Cause::Content {
                    mismatched,
                    skipped,
                },
                Cause::Content {
                    mismatched: later_mismatched,
                    skipped: later_skipped,
                },
            ) => {
                *mismatched += later_mismatched;
                *skipped += later_skipped;
            }
            (
                Cause::Timestamps {
                    backward,
                    forward_jumps,
                },
                Cause::Timestamps {
                    backward: later_backward,
                    forward_jumps: later_forward_jumps,
                },
            ) => {
                *backward += later_backward;
                *forward_jumps += later_forward_jumps;
            }
            (
                Cause::MemoryGrowth { kib_per_hour },
                Cause::MemoryGrowth {
                    kib_per_hour: later_kib_per_hour,
                },
            ) => *kib_per_hour = kib_per_hour.max(later_kib_per_hour),
            (cause, later) => unreachable!("{later:?} folded into {cause:?}"),
        }
    }
}

impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cause::Exit { code: 0 } => write!(f, "exited cleanly (end of stream)"),
            Cause::Exit { code } => write!(f, "exit code {code}"),
            Cause::Signal { signal } => write!(f, "killed by signal {signal}"),
            Cause::Stall { secs } => write!(f, "stall: no frame for {secs} s"),
            Cause::SpawnFailed { message } => write!(f, "spawn failed: {message}"),
            Cause::Content {
                mismatched,
                skipped,
            } => write!(
                f,
                "content: {mismatched} frames not in the reference, {skipped} skipped"
            ),
            Cause::Timestamps {
                backward,
                forward_jumps,
            } => write!(
                f,
                "timestamps: {backward} backward, {forward_jumps} forward jumps"
            ),
            Cause::MemoryGrowth { kib_per_hour } => {
                write!(
                    f,
                    "memory growth: rss up {kib_per_hour:.0} KiB/h over a day"
                )
            }
            Cause::NoRecovery { fault } => write!(f, "no recovery from {fault}"),
        }
    }
}

// how one faulted row came back from one fault, judged at the end of the clean window after it
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recovery {
    pub pipeline: String,
    pub fault: String,
    // unix seconds the fault ended and the clean window after it ended
    pub fault_end: u64,
    pub window_end: u64,
    // from the fault's end to the first frame of the unbroken run of right frames that lasted the clean window
    pub recovery_secs: Option<f64>,
    pub av_drift_ms: Option<f64>,
}

// opens on the first failure, closes when frames flow again
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Incident {
    pub id: String,
    pub pipeline: String,
    pub tier: Tier,
    pub start: u64,
    pub end: Option<u64>,
    pub cause: Cause,
    // restart attempts until frames flowed again
    pub restarts: u64,
    pub last_error: Option<String>,
    // stderr tail, relative to the log dir
    pub stderr: Option<String>,
}

// every open incident, rewritten on each change so an incident spans a canary restart
#[derive(Debug)]
pub struct OpenIncidents {
    log_dir: PathBuf,
    open: Vec<Incident>,
}

pub type SharedOpenIncidents = std::sync::Arc<std::sync::Mutex<OpenIncidents>>;

impl OpenIncidents {
    pub fn load(log_dir: &Path) -> Result<Self> {
        let path = log_dir.join(OPEN_INCIDENTS_FILE);
        let open = match std::fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        Ok(OpenIncidents {
            log_dir: log_dir.to_path_buf(),
            open,
        })
    }

    // a pipeline dropped from the config leaves nothing that could close its incidents
    pub fn close_unconfigured(&mut self, pipelines: &[PipelineConfig], now: u64) -> Result<()> {
        let unconfigured: Vec<(String, IncidentKind)> = self
            .open
            .iter()
            .filter(|open| {
                !pipelines
                    .iter()
                    .any(|pipeline| pipeline.id == open.pipeline)
            })
            .map(|open| (open.pipeline.clone(), open.cause.kind()))
            .collect();
        for (pipeline, kind) in unconfigured {
            self.close(&pipeline, kind, now)?;
        }
        Ok(())
    }

    pub fn all(&self) -> &[Incident] {
        &self.open
    }

    pub fn get(&self, pipeline: &str, kind: IncidentKind) -> Option<&Incident> {
        self.open
            .iter()
            .find(|incident| incident.pipeline == pipeline && incident.cause.kind() == kind)
    }

    // replaces the open incident of the same pipeline and kind
    pub fn put(&mut self, incident: Incident) -> Result<()> {
        let kind = incident.cause.kind();
        self.open
            .retain(|open| !(open.pipeline == incident.pipeline && open.cause.kind() == kind));
        self.open.push(incident);
        self.save()
    }

    pub fn close(&mut self, pipeline: &str, kind: IncidentKind, now: u64) -> Result<()> {
        let Some(position) = self
            .open
            .iter()
            .position(|open| open.pipeline == pipeline && open.cause.kind() == kind)
        else {
            return Ok(());
        };
        let mut incident = self.open.remove(position);
        incident.end = Some(now);
        // appended before the rewrite, so a crash between the two repeats the close rather than losing it
        append(&self.log_dir, INCIDENTS_PREFIX, now, &incident)?;
        self.save()
    }

    // opens or extends the incident a sample check found, closes it on a clean sample
    pub fn track(
        &mut self,
        pipeline: &PipelineConfig,
        kind: IncidentKind,
        finding: Option<Cause>,
        clean: bool,
        now: u64,
    ) -> Result<()> {
        let Some(cause) = finding else {
            return if clean {
                self.close(&pipeline.id, kind, now)
            } else {
                Ok(())
            };
        };
        let incident = match self.get(&pipeline.id, kind) {
            Some(open) => {
                let mut extended = open.clone();
                extended.cause.absorb(cause);
                extended
            }
            None => Incident {
                id: format!("{}-{}-{now}", pipeline.id, kind.label()),
                pipeline: pipeline.id.clone(),
                tier: pipeline.tier,
                start: now,
                end: None,
                cause,
                restarts: 0,
                last_error: None,
                stderr: None,
            },
        };
        self.put(incident)
    }

    fn save(&self) -> Result<()> {
        let path = self.log_dir.join(OPEN_INCIDENTS_FILE);
        let partial = self.log_dir.join(format!(".{OPEN_INCIDENTS_FILE}.partial"));
        std::fs::write(&partial, serde_json::to_string_pretty(&self.open)?)
            .with_context(|| format!("write {}", partial.display()))?;
        std::fs::rename(&partial, &path).with_context(|| format!("rename to {}", path.display()))
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs()
}

// keeps the fraction of a second that a sleep computed from `unix_now` drops
pub fn until_unix(unix_secs: u64, now: SystemTime) -> Duration {
    (UNIX_EPOCH + Duration::from_secs(unix_secs))
        .duration_since(now)
        .unwrap_or(Duration::ZERO)
}

pub fn unix_now_fractional() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs_f64()
}

pub fn utc_date(ts: u64) -> String {
    let (year, month, day) = civil_from_days(ts / SECS_PER_DAY);
    format!("{year:04}-{month:02}-{day:02}")
}

pub fn utc_datetime(ts: u64) -> String {
    let seconds_of_day = ts % SECS_PER_DAY;
    format!(
        "{} {:02}:{:02}:{:02} UTC",
        utc_date(ts),
        seconds_of_day / SECS_PER_HOUR,
        seconds_of_day % SECS_PER_HOUR / SECS_PER_MINUTE,
        seconds_of_day % SECS_PER_MINUTE
    )
}

// howard hinnant's civil_from_days, for days since 1970-01-01
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

pub fn log_file(log_dir: &Path, prefix: &str, ts: u64) -> PathBuf {
    log_dir.join(format!("{prefix}-{}.jsonl", utc_date(ts)))
}

pub fn append(log_dir: &Path, prefix: &str, ts: u64, record: &impl Serialize) -> Result<()> {
    let path = log_file(log_dir, prefix, ts);
    let mut line = serde_json::to_string(record)?;
    line.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| file.write_all(line.as_bytes()))
        .with_context(|| format!("append {}", path.display()))
}

// every record in the day files covering since..=now, the caller filters by time
pub fn read_days<T: DeserializeOwned>(
    log_dir: &Path,
    prefix: &str,
    since: u64,
    now: u64,
) -> Result<Vec<T>> {
    let mut records = Vec::new();
    for day in since / SECS_PER_DAY..=now / SECS_PER_DAY {
        let path = log_file(log_dir, prefix, day * SECS_PER_DAY);
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
        };
        for line in BufReader::new(file).lines() {
            let line = line.with_context(|| format!("read {}", path.display()))?;
            // a canary killed mid-write leaves a torn last line
            if let Ok(record) = serde_json::from_str(&line) {
                records.push(record);
            }
        }
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_until_a_unix_second_keeps_the_fraction() {
        let now = UNIX_EPOCH + Duration::from_millis(100_900);
        assert_eq!(until_unix(101, now), Duration::from_millis(100));
        assert_eq!(until_unix(100, now), Duration::ZERO);
    }

    #[test]
    fn formats_utc_dates_across_leap_days() {
        assert_eq!(utc_date(0), "1970-01-01");
        // 2024-02-29 12:34:56 UTC
        assert_eq!(utc_datetime(1_709_210_096), "2024-02-29 12:34:56 UTC");
        assert_eq!(utc_date(1_709_210_096 + SECS_PER_DAY), "2024-03-01");
        assert_eq!(utc_date(1_798_761_599), "2026-12-31");
    }

    #[test]
    fn appends_by_day_and_skips_torn_lines() {
        let dir =
            std::env::temp_dir().join(format!("calliope-canary-record-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let day_one = 1_709_210_096;
        let sample = |ts: u64| Sample {
            ts,
            pipeline: "p".into(),
            tier: Tier::SelfHosted,
            alive: true,
            frames: 30,
            fps: 30.0,
            decode_latency_ms: Some(1.5),
            rss_kb: Some(1024),
            restarts: 0,
            last_error: None,
            content: Some(ContentCounts {
                matched: 29,
                mismatched: 1,
                skipped: 0,
            }),
            timestamps: None,
            av_drift_ms: Some(-3.5),
            rss_growth_kib_per_hour: None,
        };
        append(&dir, SAMPLES_PREFIX, day_one, &sample(day_one)).unwrap();
        append(
            &dir,
            SAMPLES_PREFIX,
            day_one + SECS_PER_DAY,
            &sample(day_one + SECS_PER_DAY),
        )
        .unwrap();
        let torn = log_file(&dir, SAMPLES_PREFIX, day_one);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&torn)
            .unwrap();
        file.write_all(b"{\"ts\":12").unwrap();

        assert!(log_file(&dir, SAMPLES_PREFIX, day_one).ends_with("samples-2024-02-29.jsonl"));
        let both: Vec<Sample> =
            read_days(&dir, SAMPLES_PREFIX, day_one, day_one + SECS_PER_DAY).unwrap();
        assert_eq!(both, vec![sample(day_one), sample(day_one + SECS_PER_DAY)]);
        let second: Vec<Sample> = read_days(
            &dir,
            SAMPLES_PREFIX,
            day_one + SECS_PER_DAY,
            day_one + SECS_PER_DAY,
        )
        .unwrap();
        assert_eq!(second.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // written by the canary deployed on 2026-10-18, before the frame checks existed
    const OLD_SAMPLE: &str = r#"{"ts":1792317683,"pipeline":"baseline-rtsp","tier":"self-hosted","alive":true,"frames":1682,"fps":28.03296596738814,"decode_latency_ms":4.194304,"rss_kb":51852,"restarts":2,"last_error":"pipeline error: Hardware(Other)"}"#;

    #[test]
    fn reads_samples_written_before_the_frame_checks() {
        let sample: Sample = serde_json::from_str(OLD_SAMPLE).unwrap();
        assert_eq!(sample.frames, 1682);
        assert_eq!(sample.content, None);
        assert_eq!(sample.rss_growth_kib_per_hour, None);
    }

    fn checked_pipeline() -> PipelineConfig {
        PipelineConfig {
            id: "baseline-rtsp".into(),
            tier: Tier::SelfHosted,
            launch: "rtspsrc location=rtsp://127.0.0.1:18554/baseline ! decodebin ! checksumsink name=video".into(),
            ends: false,
            video_sink: Some("video".into()),
            audio_sink: None,
            expect_hashes: None,
        }
    }

    #[test]
    fn check_incidents_accumulate_survive_a_reload_and_close_on_a_clean_sample() {
        let dir = std::env::temp_dir().join(format!("calliope-canary-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pipeline = checked_pipeline();
        let start = 1_792_317_683;
        let content = |mismatched, skipped| Cause::Content {
            mismatched,
            skipped,
        };
        let mut open = OpenIncidents::load(&dir).unwrap();
        open.track(
            &pipeline,
            IncidentKind::Content,
            Some(content(2, 0)),
            false,
            start,
        )
        .unwrap();
        // a sample with no frames neither extends nor closes it
        open.track(&pipeline, IncidentKind::Content, None, false, start + 60)
            .unwrap();
        open.track(
            &pipeline,
            IncidentKind::Content,
            Some(content(1, 5)),
            false,
            start + 120,
        )
        .unwrap();

        let mut reloaded = OpenIncidents::load(&dir).unwrap();
        let incident = reloaded.get(&pipeline.id, IncidentKind::Content).unwrap();
        assert_eq!(incident.id, format!("baseline-rtsp-content-{start}"));
        assert_eq!(incident.cause, content(3, 5));
        assert!(reloaded.get(&pipeline.id, IncidentKind::Down).is_none());

        reloaded
            .track(&pipeline, IncidentKind::Content, None, true, start + 180)
            .unwrap();
        assert!(OpenIncidents::load(&dir).unwrap().all().is_empty());
        let closed: Vec<Incident> = read_days(&dir, INCIDENTS_PREFIX, start, start + 180).unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].end, Some(start + 180));

        reloaded
            .track(
                &pipeline,
                IncidentKind::Content,
                Some(content(1, 0)),
                false,
                start + 240,
            )
            .unwrap();
        let mut removed_from_config = OpenIncidents::load(&dir).unwrap();
        removed_from_config
            .close_unconfigured(&[], start + 300)
            .unwrap();
        assert!(OpenIncidents::load(&dir).unwrap().all().is_empty());
        let closed: Vec<Incident> = read_days(&dir, INCIDENTS_PREFIX, start, start + 300).unwrap();
        assert_eq!(closed.last().unwrap().end, Some(start + 300));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
