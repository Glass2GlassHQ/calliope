use std::fmt;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::config::Tier;

pub const SECS_PER_DAY: u64 = 86_400;
const SECS_PER_HOUR: u64 = 3_600;
const SECS_PER_MINUTE: u64 = 60;
pub const SAMPLES_PREFIX: &str = "samples";
pub const INCIDENTS_PREFIX: &str = "incidents";
pub const STDERR_DIR: &str = "stderr";

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
}

impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cause::Exit { code: 0 } => write!(f, "exited cleanly (end of stream)"),
            Cause::Exit { code } => write!(f, "exit code {code}"),
            Cause::Signal { signal } => write!(f, "killed by signal {signal}"),
            Cause::Stall { secs } => write!(f, "stall: no frame for {secs} s"),
            Cause::SpawnFailed { message } => write!(f, "spawn failed: {message}"),
        }
    }
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

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs()
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
}
