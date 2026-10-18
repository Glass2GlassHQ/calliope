use std::collections::BTreeMap;
use std::path::Path;

use calliope_core::engine::binary;
use calliope_core::produce::{Validation, Validator};
use serde::Deserialize;

use crate::run_tool;

const DEFAULT_WRAPPER: &str = "tools/validators/dashif.sh";
const FAIL_STATE: &str = "FAIL";
const WARN_STATE: &str = "WARN";

// part (MPD, Segments) -> spec -> section -> its checks
type Report = BTreeMap<String, BTreeMap<String, BTreeMap<String, Section>>>;

#[derive(Debug, Deserialize)]
struct Section {
    #[serde(default)]
    checks: BTreeMap<String, Check>,
}

#[derive(Debug, Deserialize)]
struct Check {
    state: String,
}

// the conformance tool exits 0 whatever it finds, only the json states tell
pub fn validate(artifact: &Path) -> Validation {
    let program = binary("CALLIOPE_DASHIF", DEFAULT_WRAPPER);
    let output = match run_tool(Validator::DashIf, &program, &[], artifact) {
        Ok(output) => output,
        Err(skipped) => return skipped,
    };
    match serde_json::from_str::<Report>(&output.stdout) {
        Ok(report) => verdict(&report),
        Err(e) => Validation::skipped(
            Validator::DashIf,
            format!("no conformance report ({e}): {}", output.stderr.trim()),
        ),
    }
}

// failed checks fail the manifest, warnings ride along as messages
fn verdict(report: &Report) -> Validation {
    let with_state = |wanted: &str| {
        let mut lines = Vec::new();
        for (part, specs) in report {
            for (spec, sections) in specs {
                for (section, body) in sections {
                    for (statement, check) in &body.checks {
                        if check.state == wanted {
                            lines.push(format!("{wanted} {part} {spec} {section}: {statement}"));
                        }
                    }
                }
            }
        }
        lines
    };
    let failures = with_state(FAIL_STATE);
    let failed = !failures.is_empty();
    let messages = failures.into_iter().chain(with_state(WARN_STATE)).collect();
    if failed {
        Validation::fail(Validator::DashIf, messages)
    } else {
        Validation::pass(Validator::DashIf, messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use calliope_core::produce::Verdict;

    #[test]
    fn a_segment_longer_than_max_segment_duration_fails() {
        let report: Report =
            serde_json::from_str(include_str!("../testdata/dashif-ffmpeg.json")).unwrap();
        let v = verdict(&report);
        assert_eq!(v.verdict, Verdict::Fail);
        assert_eq!(v.messages.len(), 1, "{:?}", v.messages);
        assert!(v.messages[0].contains("maxSegmentDuration"));
    }
}
