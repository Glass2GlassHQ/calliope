//! ffmpeg adapter. ffmpeg emits framemd5 natively, so it is also the usual
//! reference engine.

use std::path::Path;

use calliope_core::Result;
use calliope_core::engine::{Engine, EngineInfo, Invocation, OutputSpec, binary, probe_first_line};
use calliope_core::scenario::Scenario;

#[derive(Debug, Default)]
pub struct Ffmpeg;

impl Engine for Ffmpeg {
    fn id(&self) -> &'static str {
        "ffmpeg"
    }

    fn probe(&self) -> Result<EngineInfo> {
        probe_first_line(
            self.id(),
            &binary("CALLIOPE_FFMPEG", "ffmpeg"),
            &["-version"],
        )
    }

    fn plan(&self, _scenario: &Scenario, input: &Path, workdir: &Path) -> Result<Invocation> {
        let out = workdir.join("out.framemd5");
        Ok(Invocation {
            program: binary("CALLIOPE_FFMPEG", "ffmpeg"),
            args: vec![
                "-nostdin".into(),
                "-hide_banner".into(),
                "-y".into(),
                "-i".into(),
                input.display().to_string(),
                "-an".into(),
                "-f".into(),
                "framemd5".into(),
                out.display().to_string(),
            ],
            output: OutputSpec::FrameMd5File(out),
        })
    }
}
