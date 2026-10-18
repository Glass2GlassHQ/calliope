use std::path::Path;

use calliope_core::engine::binary;
use calliope_core::produce::{Validation, Validator};

use crate::run_tool;

const ERROR_PREFIX: &str = "ERR";
const WARNING_PREFIX: &str = "WRN";

// mkvalidator 0.6.0 exits 0 on a file with errors, only its ERR lines tell
pub fn validate(artifact: &Path) -> Validation {
    let program = binary("CALLIOPE_MKVALIDATOR", "mkvalidator");
    match run_tool(Validator::Mkvalidator, &program, &["--quiet"], artifact) {
        Ok(output) => verdict(&output.stderr),
        Err(skipped) => skipped,
    }
}

// errors fail the file, warnings ride along as messages
fn verdict(log: &str) -> Validation {
    let lines: Vec<&str> = log
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let with_prefix = |prefix: &str| {
        lines
            .iter()
            .filter(|l| l.starts_with(prefix))
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
    };
    let errors = with_prefix(ERROR_PREFIX);
    let warnings = with_prefix(WARNING_PREFIX);
    let failed = !errors.is_empty();
    let messages = errors.into_iter().chain(warnings).collect();
    if failed {
        Validation::fail(Validator::Mkvalidator, messages)
    } else {
        Validation::pass(Validator::Mkvalidator, messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use calliope_core::produce::Verdict;

    #[test]
    fn warnings_alone_pass() {
        let v = verdict(include_str!("../testdata/mkvalidator-valid.stderr"));
        assert_eq!(v.verdict, Verdict::Pass);
        assert_eq!(v.messages.len(), 1, "{:?}", v.messages);
    }

    #[test]
    fn missing_mandatory_elements_fail() {
        let v = verdict(include_str!("../testdata/mkvalidator-missing-apps.stderr"));
        assert_eq!(v.verdict, Verdict::Fail);
        assert!(v.messages[0].starts_with("ERR200: Missing element 'MuxingApp'"));
    }

    #[test]
    fn a_truncated_file_fails() {
        let v = verdict(include_str!("../testdata/mkvalidator-truncated.stderr"));
        assert_eq!(v.verdict, Verdict::Fail);
        assert!(v.messages.iter().any(|m| m.contains("Invalid checksum")));
    }
}
