//! Serializable results: one report per invocation, grouped by scenario,
//! written as JSON so regression tracking can diff runs across machines.

use serde::Serialize;

use crate::compare::{Comparison, MajorityVote};
use crate::runner::{RunResult, RunStatus};

#[derive(Debug, Serialize)]
pub struct Report {
    pub scenarios: Vec<ScenarioReport>,
}

#[derive(Debug, Serialize)]
pub struct ScenarioReport {
    pub scenario: String,
    pub reference: String,
    /// robustness scenario: judged on graceful degradation, not frame equality
    pub robustness: bool,
    /// soak scenario: judged on stability across repeated iterations
    pub soak: bool,
    /// determinism scenario: judged on byte-identical output across repeated runs
    pub determinism: bool,
    /// golden scenario: the conformance hash every engine's output must match
    #[serde(skip_serializing_if = "Option::is_none")]
    pub golden_expected: Option<String>,
    /// differential scenario with >=3 engines: which engine(s) are the outlier
    /// when they diverge, so a divergence points at the culprit
    #[serde(skip_serializing_if = "Option::is_none")]
    pub majority: Option<MajorityVote>,
    pub runs: Vec<EngineReport>,
}

#[derive(Debug, Serialize)]
pub struct EngineReport {
    #[serde(flatten)]
    pub run: RunResult,
    /// None for the reference engine and for runs that produced no hashes
    pub comparison: Option<Comparison>,
}

impl ScenarioReport {
    pub fn passed(&self) -> bool {
        // golden: every engine's whole-output MD5 must equal the conformance hash
        if let Some(expected) = &self.golden_expected {
            return self.runs.iter().all(|r| {
                matches!(r.run.status, RunStatus::Ok)
                    && r.run
                        .output_md5
                        .as_ref()
                        .is_some_and(|got| got.eq_ignore_ascii_case(expected))
            });
        }
        // determinism: every engine's output was byte-identical across its runs
        if self.determinism {
            return self.runs.iter().all(|r| {
                matches!(r.run.status, RunStatus::Ok) && r.run.determinism_matched == Some(true)
            });
        }
        // robustness and soak both pass on graceful survival (no crash / hang),
        // differential on Ok status plus a matching frame comparison
        if self.robustness || self.soak {
            return self
                .runs
                .iter()
                .all(|r| r.run.status.survived_corrupt_input());
        }
        self.runs.iter().all(|r| {
            matches!(r.run.status, RunStatus::Ok) && r.comparison.as_ref().is_none_or(|c| c.matched)
        })
    }
}

impl Report {
    pub fn passed(&self) -> bool {
        self.scenarios.iter().all(ScenarioReport::passed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::RunResult;
    use std::path::PathBuf;

    fn golden_run(engine: &str, md5: Option<&str>) -> EngineReport {
        EngineReport {
            run: RunResult {
                engine: engine.into(),
                status: RunStatus::Ok,
                duration_ms: 0,
                peak_rss_kb: None,
                frame_hashes: None,
                log_dir: PathBuf::new(),
                iterations_completed: None,
                output_md5: md5.map(str::to_string),
                determinism_matched: None,
            },
            comparison: None,
        }
    }

    fn golden_report(runs: Vec<EngineReport>) -> ScenarioReport {
        ScenarioReport {
            scenario: "g".into(),
            reference: "ffmpeg".into(),
            robustness: false,
            soak: false,
            determinism: false,
            golden_expected: Some("ABCD".into()),
            majority: None,
            runs,
        }
    }

    #[test]
    fn golden_passes_only_when_every_engine_matches_the_hash() {
        // all match (case-insensitive) -> pass
        let ok = golden_report(vec![
            golden_run("ffmpeg", Some("abcd")),
            golden_run("g2g", Some("ABCD")),
        ]);
        assert!(ok.passed());

        // one engine diverges from the conformance hash -> fail
        let bad = golden_report(vec![
            golden_run("ffmpeg", Some("abcd")),
            golden_run("g2g", Some("dead")),
        ]);
        assert!(!bad.passed());

        // an engine that produced no hash -> fail
        let missing = golden_report(vec![golden_run("ffmpeg", None)]);
        assert!(!missing.passed());
    }
}
