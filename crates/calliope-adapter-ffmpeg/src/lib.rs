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

    fn plan(&self, scenario: &Scenario, input: &Path, workdir: &Path) -> Result<Invocation> {
        let program = binary("CALLIOPE_FFMPEG", "ffmpeg");
        let mut args = vec![
            "-nostdin".into(),
            "-hide_banner".into(),
            "-y".into(),
            "-i".into(),
            input.display().to_string(),
            "-an".into(),
        ];
        // golden: whole decoded output as raw video in the vector's format, so
        // the runner's whole-file MD5 reproduces the conformance `result`
        // (Fluster's `-vf format=<fmt> -f rawvideo`). Otherwise per-frame md5.
        let output = if scenario.is_golden() {
            let fmt = scenario
                .video
                .map_or("yuv420p", |v| v.format.ffmpeg_pix_fmt());
            let out = workdir.join("out.rawvideo");
            args.extend([
                "-vf".into(),
                format!("format={fmt}"),
                "-f".into(),
                "rawvideo".into(),
                out.display().to_string(),
            ]);
            OutputSpec::RawVideoFile(out)
        } else {
            let out = workdir.join("out.framemd5");
            args.extend(["-f".into(), "framemd5".into(), out.display().to_string()]);
            OutputSpec::FrameMd5File(out)
        };
        Ok(Invocation {
            program,
            args,
            output,
        })
    }
}
