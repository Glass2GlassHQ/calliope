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
        // golden: every engine's whole-output MD5 must equal the conformance hash.
        if let Some(expected) = &self.golden_expected {
            // gstreamer's avdec emits an alignment-cropped geometry on cropped
            // streams (a gst-libav limitation, not a decode bug), so its output
            // can't match the tightly-cropped conformance hash. Detect that by
            // output length: when gstreamer's differs from the reference engine's,
            // exclude gstreamer from the verdict rather than failing a vector the
            // real engines pass. Every other engine (and gstreamer on a same-size
            // stream) stays held to the hash, so a genuine divergence still fails.
            let ref_len = self
                .runs
                .iter()
                .find(|r| r.run.engine == self.reference)
                .and_then(|r| r.run.output_len);
            return self.runs.iter().all(|r| {
                let gst_crop_excluded =
                    r.run.engine == "gstreamer" && ref_len.is_some() && r.run.output_len != ref_len;
                gst_crop_excluded
                    || (matches!(r.run.status, RunStatus::Ok)
                        && r.run
                            .output_md5
                            .as_ref()
                            .is_some_and(|got| got.eq_ignore_ascii_case(expected)))
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
        golden_run_len(engine, md5, Some(100))
    }

    fn golden_run_len(engine: &str, md5: Option<&str>, len: Option<u64>) -> EngineReport {
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
                output_len: len,
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

    #[test]
    fn golden_excludes_gstreamer_when_its_output_geometry_diverges() {
        // gstreamer's avdec under-crops (alignment), so its output length differs
        // from the reference and its hash cannot match: excluded, vector passes.
        let cropped = golden_report(vec![
            golden_run_len("ffmpeg", Some("abcd"), Some(300)),
            golden_run_len("g2g", Some("abcd"), Some(300)),
            golden_run_len("gstreamer", Some("dead"), Some(326)),
        ]);
        assert!(
            cropped.passed(),
            "gstreamer crop-geometry mismatch is excluded"
        );

        // A same-length gstreamer mismatch is a real divergence -> still fails.
        let real = golden_report(vec![
            golden_run_len("ffmpeg", Some("abcd"), Some(300)),
            golden_run_len("gstreamer", Some("dead"), Some(300)),
        ]);
        assert!(
            !real.passed(),
            "same-geometry gstreamer mismatch still fails"
        );

        // g2g is never excluded: a size divergence (e.g. dropped frames) fails.
        let g2g_bug = golden_report(vec![
            golden_run_len("ffmpeg", Some("abcd"), Some(300)),
            golden_run_len("g2g", Some("dead"), Some(243)),
        ]);
        assert!(!g2g_bug.passed(), "g2g geometry divergence must still fail");
    }
}
