mod dashif;
mod ffprobe;
mod hls;
mod mediastreamvalidator;
mod mkvalidator;
mod mp4box;
mod player;
mod tsduck;

use std::path::Path;
use std::process::{Command, ExitStatus};

use calliope_core::engine::OutputSpec;
use calliope_core::produce::{Expectation, Produce, Validation, Validator};

pub fn validate(
    validator: Validator,
    produce: &Produce,
    output: &OutputSpec,
    expected: &Expectation,
) -> Validation {
    if validator == Validator::Player {
        return player::validate(produce.container, output, expected);
    }
    let OutputSpec::Produced(artifact) = output else {
        return Validation::skipped(validator, "the engine wrote no file to check".into());
    };
    // a relative manifest path breaks segment lookup in ffprobe's dash demuxer
    let artifact = &match std::path::absolute(artifact) {
        Ok(path) => path,
        Err(e) => return Validation::skipped(validator, format!("{}: {e}", artifact.display())),
    };
    match validator {
        Validator::Ffprobe => ffprobe::validate(artifact, produce.container, expected),
        Validator::Mp4box => mp4box::validate(artifact, produce.container),
        Validator::Mkvalidator => mkvalidator::validate(artifact),
        Validator::Tsduck => tsduck::validate(artifact, produce.container),
        Validator::DashIf => dashif::validate(artifact),
        Validator::Mediastreamvalidator => mediastreamvalidator::validate(artifact),
        Validator::Player => unreachable!("handled above"),
    }
}

#[derive(Debug)]
struct ToolOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

// a validator missing from this host is skipped, never a finding against the output
fn run_tool(
    validator: Validator,
    program: &str,
    args: &[&str],
    artifact: &Path,
) -> Result<ToolOutput, Validation> {
    run_command(validator, Command::new(program).args(args).arg(artifact))
}

fn run_command(validator: Validator, command: &mut Command) -> Result<ToolOutput, Validation> {
    let program = command.get_program().to_string_lossy().into_owned();
    match command.output() {
        Ok(out) => Ok(ToolOutput {
            status: out.status,
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }),
        Err(e) => Err(Validation::skipped(validator, format!("{program}: {e}"))),
    }
}
