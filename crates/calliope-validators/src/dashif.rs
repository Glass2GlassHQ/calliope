use std::path::Path;

use calliope_core::engine::binary;
use calliope_core::produce::{Validation, Validator};

use crate::run_tool;

const DEFAULT_WRAPPER: &str = "tools/validators/dashif.sh";

pub fn validate(artifact: &Path) -> Validation {
    let program = binary("CALLIOPE_DASHIF", DEFAULT_WRAPPER);
    match run_tool(Validator::DashIf, &program, &[], artifact) {
        Ok(output) => Validation::from_findings(Validator::DashIf, vec![output.stdout]),
        Err(skipped) => skipped,
    }
}
