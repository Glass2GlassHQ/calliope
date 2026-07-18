//! The engine adapter boundary. Adapters are declarative: they map a
//! scenario to a subprocess invocation and say what artifact it produces.
//! The runner owns execution (timeout, logs, RSS), keeping adapters trivial.

use std::path::{Path, PathBuf};

use crate::scenario::Scenario;
use crate::{Error, Result};

pub trait Engine: Send + Sync {
    fn id(&self) -> &'static str;

    /// detect the engine binary and report its version; Err means absent
    fn probe(&self) -> Result<EngineInfo>;

    /// map a scenario to one subprocess run; outputs go under `workdir`
    fn plan(&self, scenario: &Scenario, input: &Path, workdir: &Path) -> Result<Invocation>;

    /// A determinism-mode variant that must produce byte-identical output to
    /// [`Self::plan`]. `None` (the default) means the engine has no such
    /// variant, so only the repeated base runs are compared. g2g returns a
    /// `--threads` build of the same pipeline to catch threading-order bugs.
    fn threaded_plan(
        &self,
        _scenario: &Scenario,
        _input: &Path,
        _workdir: &Path,
    ) -> Result<Option<Invocation>> {
        Ok(None)
    }
}

#[derive(Debug, Clone)]
pub struct EngineInfo {
    pub id: String,
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    pub output: OutputSpec,
}

#[derive(Debug, Clone)]
pub enum OutputSpec {
    /// engine wrote ffmpeg framemd5 format itself
    FrameMd5File(PathBuf),
    /// engine wrote concatenated raw frames; the runner hashes per frame
    /// using the scenario's video geometry
    RawVideoFile(PathBuf),
    /// engine wrote an encoded elementary stream (a roundtrip transcode); the
    /// runner leaves it for ffmpeg to decode + PSNR, not hashed here
    EncodedFile(PathBuf),
}

/// binary override hook: `env_var` (e.g. CALLIOPE_FFMPEG) wins over `default`
pub fn binary(env_var: &str, default: &str) -> String {
    std::env::var(env_var).unwrap_or_else(|_| default.to_string())
}

/// shared probe helper: run `program args...`, return first output line
pub fn probe_first_line(id: &str, program: &str, args: &[&str]) -> Result<EngineInfo> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| Error::Engine {
            engine: id.into(),
            message: format!("{program}: {e}"),
        })?;
    if !out.status.success() {
        return Err(Error::Engine {
            engine: id.into(),
            message: format!("{program} exited with {}", out.status),
        });
    }
    let text = if out.stdout.is_empty() {
        out.stderr
    } else {
        out.stdout
    };
    let first = String::from_utf8_lossy(&text)
        .lines()
        .next()
        .unwrap_or("unknown")
        .to_string();
    Ok(EngineInfo {
        id: id.into(),
        version: first,
    })
}
