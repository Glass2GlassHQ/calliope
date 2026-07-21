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
    /// audio scenario: the whole PCM stream is one hash, so the summary counts
    /// streams, not frames (frame boundaries differ across decoders)
    pub audio: bool,
    /// outcome-diff scenario: a robustness run that also cross-compares decode
    /// outcomes. Judged like robustness (crash / hang fails); the decode-outcome
    /// divergences it surfaces are advisory triage, not a pass / fail gate.
    pub outcome_diff: bool,
    /// golden scenario: the conformance hash every engine's output must match
    #[serde(skip_serializing_if = "Option::is_none")]
    pub golden_expected: Option<String>,
    /// differential scenario with >=3 engines: which engine(s) are the outlier
    /// when they diverge, so a divergence points at the culprit
    #[serde(skip_serializing_if = "Option::is_none")]
    pub majority: Option<MajorityVote>,
    /// roundtrip scenario: the minimum PSNR (dB) every engine's re-encoded stream
    /// must reach (each engine's actual PSNR rides its `RunResult`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roundtrip_psnr_min: Option<f64>,
    /// resolution-change scenario: the expected total decoded byte count (sum of
    /// the per-frame packed sizes from ffprobe) every engine's output must match.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution_change_expected_bytes: Option<u64>,
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
        // roundtrip: every engine encoded a decodable stream at >= the PSNR floor
        if let Some(min) = self.roundtrip_psnr_min {
            return self.runs.iter().all(|r| {
                matches!(r.run.status, RunStatus::Ok) && r.run.psnr.is_some_and(|p| p >= min)
            });
        }
        // resolution-change: every engine survived and emitted exactly the
        // expected decoded byte total (a crash / hang, dropped frames, or a
        // frozen / wrong resolution all change the count).
        if let Some(expected) = self.resolution_change_expected_bytes {
            return self.runs.iter().all(|r| {
                matches!(r.run.status, RunStatus::Ok) && r.run.output_len == Some(expected)
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
                psnr: None,
                decoded: None,
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
            audio: false,
            outcome_diff: false,
            golden_expected: Some("ABCD".into()),
            majority: None,
            roundtrip_psnr_min: None,
            resolution_change_expected_bytes: None,
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

    fn roundtrip_run(engine: &str, status: RunStatus, psnr: Option<f64>) -> EngineReport {
        let mut r = golden_run(engine, None);
        r.run.status = status;
        r.run.psnr = psnr;
        r
    }

    fn roundtrip_report(min: f64, runs: Vec<EngineReport>) -> ScenarioReport {
        let mut s = golden_report(runs);
        s.golden_expected = None;
        s.roundtrip_psnr_min = Some(min);
        s
    }

    fn reschange_run(engine: &str, status: RunStatus, len: Option<u64>) -> EngineReport {
        let mut r = golden_run_len(engine, None, len);
        r.run.status = status;
        r
    }

    fn reschange_report(expected: u64, runs: Vec<EngineReport>) -> ScenarioReport {
        let mut s = golden_report(runs);
        s.golden_expected = None;
        s.resolution_change_expected_bytes = Some(expected);
        s
    }

    #[test]
    fn resolution_change_passes_on_exact_byte_total_and_fails_otherwise() {
        assert!(
            reschange_report(3000, vec![reschange_run("g2g", RunStatus::Ok, Some(3000))]).passed(),
            "exact expected byte total passes"
        );
        // fewer bytes (dropped frames / frozen resolution) fails
        assert!(
            !reschange_report(3000, vec![reschange_run("g2g", RunStatus::Ok, Some(1200))]).passed(),
            "wrong byte total fails"
        );
        // a crash during renegotiation fails regardless of bytes
        assert!(
            !reschange_report(
                3000,
                vec![reschange_run(
                    "g2g",
                    RunStatus::Signaled { signal: 6 },
                    Some(3000)
                )]
            )
            .passed(),
            "a crash fails"
        );
    }

    #[test]
    fn roundtrip_passes_above_the_psnr_floor_and_fails_below_or_on_crash() {
        assert!(
            roundtrip_report(30.0, vec![roundtrip_run("g2g", RunStatus::Ok, Some(45.0))]).passed(),
            "PSNR above the floor passes"
        );
        assert!(
            !roundtrip_report(30.0, vec![roundtrip_run("g2g", RunStatus::Ok, Some(12.0))]).passed(),
            "PSNR below the floor fails"
        );
        assert!(
            !roundtrip_report(30.0, vec![roundtrip_run("g2g", RunStatus::Ok, None)]).passed(),
            "a missing PSNR (undecodable encode) fails"
        );
        assert!(
            !roundtrip_report(
                30.0,
                vec![roundtrip_run("g2g", RunStatus::Timeout, Some(99.0))]
            )
            .passed(),
            "a crash / hang fails regardless of PSNR"
        );
    }
}
