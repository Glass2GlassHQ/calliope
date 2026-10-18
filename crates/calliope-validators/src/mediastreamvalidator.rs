use std::path::Path;

use calliope_core::engine::binary;
use calliope_core::produce::{Validation, Validator};

use crate::run_tool;

// Apple ships mediastreamvalidator with its macOS HTTP Live Streaming tools only
pub fn validate(artifact: &Path) -> Validation {
    if !cfg!(target_os = "macos") {
        return Validation::skipped(
            Validator::Mediastreamvalidator,
            "mediastreamvalidator runs on macOS only".into(),
        );
    }
    let program = binary("CALLIOPE_MEDIASTREAMVALIDATOR", "mediastreamvalidator");
    let output = match run_tool(Validator::Mediastreamvalidator, &program, &[], artifact) {
        Ok(output) => output,
        Err(skipped) => return skipped,
    };
    // TODO: parse its report, only the exit status is judged until a mac run captures one
    if output.status.success() {
        Validation::pass(Validator::Mediastreamvalidator, Vec::new())
    } else {
        Validation::fail(
            Validator::Mediastreamvalidator,
            vec![format!(
                "mediastreamvalidator exited with {}",
                output.status
            )],
        )
    }
}
