use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use calliope_core::engine::binary;
use calliope_core::produce::{Container, Validation, Validator};
use serde::Deserialize;

use crate::run_command;

const PCR_CLOCK_HZ: f64 = 27_000_000.0;
// ISO/IEC 13818-1 2.7.2: PCRs at most 100 ms apart
const MAX_PCR_INTERVAL: Duration = Duration::from_millis(100);
const TSP_ERROR_PREFIX: &str = "* Error";
const TSP_WARNING_PREFIX: &str = "* Warning";
const ANALYSIS_SUFFIX: &str = "analyze.json";
const PCR_SUFFIX: &str = "pcr.csv";
// tsp otherwise checks the network for a newer release on every run
const NO_VERSION_CHECK_VARIABLE: &str = "TSDUCK_NO_VERSION_CHECK";

#[derive(Debug, Deserialize)]
struct Analysis {
    ts: TransportStream,
    pids: Vec<Pid>,
}

#[derive(Debug, Deserialize)]
struct TransportStream {
    packets: TransportPackets,
    pids: PidSummary,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct TransportPackets {
    invalid_syncs: u64,
    transport_errors: u64,
}

#[derive(Debug, Deserialize)]
struct PidSummary {
    pcr: u64,
    unreferenced: u64,
}

#[derive(Debug, Deserialize)]
struct Pid {
    id: u64,
    packets: PidPackets,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct PidPackets {
    discontinuities: u64,
    duplicated: u64,
    pcr_leap: u64,
    pts_leap: u64,
    dts_leap: u64,
}

// one tsp pass per file: the analyze plugin's json report plus every PCR value
pub fn validate(artifact: &Path, container: Container) -> Validation {
    let files = if container == Container::HlsTs {
        match crate::hls::segments(artifact) {
            Ok(segments) => segments,
            Err(finding) => return Validation::fail(Validator::Tsduck, vec![finding]),
        }
    } else {
        vec![artifact.to_path_buf()]
    };
    let program = binary("CALLIOPE_TSP", "tsp");
    let mut findings = Vec::new();
    for file in &files {
        let name = file.file_name().unwrap_or_default().to_string_lossy();
        let analysis = file.with_extension(ANALYSIS_SUFFIX);
        let pcr = file.with_extension(PCR_SUFFIX);
        let mut command = Command::new(&program);
        command
            .env(NO_VERSION_CHECK_VARIABLE, "1")
            .args(["-I", "file"])
            .arg(file)
            .args(["-P", "analyze", "--json", "-o"])
            .arg(&analysis)
            .args(["-P", "pcrextract", "--pcr", "--csv", "-o"])
            .arg(&pcr)
            .args(["-O", "drop"]);
        let output = match run_command(Validator::Tsduck, &mut command) {
            Ok(output) => output,
            Err(skipped) => return skipped,
        };
        let mut file_findings = log_findings(&output.stderr);
        match std::fs::read_to_string(&analysis) {
            Ok(json) => file_findings.extend(analysis_findings(&json)),
            Err(e) => file_findings.push(format!("no analyze report: {e}")),
        }
        if let Ok(csv) = std::fs::read_to_string(&pcr) {
            file_findings.extend(pcr_findings(&csv));
        }
        findings.extend(file_findings.into_iter().map(|f| format!("{name}: {f}")));
    }
    Validation::from_findings(Validator::Tsduck, findings)
}

fn log_findings(log: &str) -> Vec<String> {
    log.lines()
        .map(str::trim)
        .filter(|l| l.starts_with(TSP_ERROR_PREFIX) || l.starts_with(TSP_WARNING_PREFIX))
        .map(str::to_string)
        .collect()
}

fn analysis_findings(json: &str) -> Vec<String> {
    let analysis: Analysis = match serde_json::from_str(json) {
        Ok(analysis) => analysis,
        Err(e) => return vec![format!("unreadable analyze report: {e}")],
    };
    let mut findings = Vec::new();
    let packets = &analysis.ts.packets;
    if packets.invalid_syncs > 0 {
        findings.push(format!("{} invalid sync bytes", packets.invalid_syncs));
    }
    if packets.transport_errors > 0 {
        findings.push(format!("{} transport errors", packets.transport_errors));
    }
    if analysis.ts.pids.pcr == 0 {
        findings.push("no PID carries a PCR".into());
    }
    if analysis.ts.pids.unreferenced > 0 {
        findings.push(format!(
            "{} PIDs no table references",
            analysis.ts.pids.unreferenced
        ));
    }
    for pid in &analysis.pids {
        let p = &pid.packets;
        for (count, what) in [
            (p.discontinuities, "continuity counter discontinuities"),
            (p.duplicated, "duplicated packets"),
            (p.pcr_leap, "PCR leaps"),
            (p.pts_leap, "PTS leaps"),
            (p.dts_leap, "DTS leaps"),
        ] {
            if count > 0 {
                findings.push(format!("PID {:#06x}: {count} {what}", pid.id));
            }
        }
    }
    findings
}

// pcrextract csv: PID first, the 27 MHz PCR value in the sixth column
fn pcr_findings(csv: &str) -> Vec<String> {
    const PID_COLUMN: usize = 0;
    const VALUE_COLUMN: usize = 5;
    let mut last: HashMap<&str, u64> = HashMap::new();
    let mut widest: HashMap<&str, f64> = HashMap::new();
    for line in csv.lines().skip(1) {
        let columns: Vec<&str> = line.split(',').collect();
        let (Some(pid), Some(Ok(value))) = (
            columns.get(PID_COLUMN),
            columns.get(VALUE_COLUMN).map(|v| v.parse::<u64>()),
        ) else {
            continue;
        };
        if let Some(previous) = last.insert(pid, value) {
            let seconds = value.saturating_sub(previous) as f64 / PCR_CLOCK_HZ;
            let entry = widest.entry(pid).or_default();
            *entry = entry.max(seconds);
        }
    }
    let mut findings: Vec<String> = widest
        .into_iter()
        .filter(|(_, seconds)| *seconds > MAX_PCR_INTERVAL.as_secs_f64())
        .map(|(pid, seconds)| {
            format!(
                "PID {pid}: PCRs up to {} ms apart, the limit is {} ms",
                Duration::from_secs_f64(seconds).as_millis(),
                MAX_PCR_INTERVAL.as_millis()
            )
        })
        .collect();
    findings.sort();
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_stream_has_no_findings() {
        assert!(analysis_findings(include_str!("../testdata/tsduck-clean.json")).is_empty());
        assert!(pcr_findings(include_str!("../testdata/tsduck-clean-pcr.csv")).is_empty());
        assert!(log_findings(include_str!("../testdata/tsduck-clean.stderr")).is_empty());
    }

    #[test]
    fn dropped_packets_break_continuity() {
        let findings = analysis_findings(include_str!("../testdata/tsduck-dropped-packets.json"));
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(findings.iter().all(|f| f.contains("discontinuities")));
    }

    #[test]
    fn lost_sync_is_logged() {
        let findings = log_findings(include_str!("../testdata/tsduck-truncated.stderr"));
        assert_eq!(findings.len(), 1);
        assert!(findings[0].contains("synchronization lost"));
    }

    #[test]
    fn sparse_pcrs_exceed_the_interval() {
        let findings = pcr_findings(include_str!("../testdata/tsduck-sparse-pcr.csv"));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].starts_with("PID 256"));
    }
}
