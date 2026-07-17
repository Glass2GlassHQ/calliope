//! glass2glass adapter, driving `g2g-launch` (the gst-launch analog) as a
//! subprocess like every other engine: decode to a raw I420 dump the runner
//! hashes. g2g-launch is not usually on PATH; point CALLIOPE_G2G_LAUNCH at a
//! built binary.

use std::path::Path;

use calliope_core::Result;
use calliope_core::engine::{Engine, EngineInfo, Invocation, OutputSpec, binary, probe_first_line};
use calliope_core::scenario::Scenario;

#[derive(Debug, Default)]
pub struct G2g;

impl Engine for G2g {
    fn id(&self) -> &'static str {
        "g2g"
    }

    fn probe(&self) -> Result<EngineInfo> {
        // g2g-launch has no --version; a successful --help means present
        probe_first_line(
            self.id(),
            &binary("CALLIOPE_G2G_LAUNCH", "g2g-launch"),
            &["--help"],
        )
    }

    fn plan(&self, scenario: &Scenario, input: &Path, workdir: &Path) -> Result<Invocation> {
        let out = workdir.join("out.yuv");
        // Pin the decoded format with a capsfilter (GStreamer fourcc names) so
        // ffmpegdec's Auto output resolves to it and the raw dump matches
        // ffmpeg's native framemd5. The decoder emits it directly, no convert.
        let format = scenario.video.map_or("I420", |v| v.format.gst_format());
        let pipeline = format!(
            "filesrc location={} ! decodebin ! video/x-raw,format={format} ! filesink location={}",
            input.display(),
            out.display()
        );
        let mut args = vec!["-q".to_string()];
        args.extend(pipeline.split(' ').map(str::to_string));
        Ok(Invocation {
            program: binary("CALLIOPE_G2G_LAUNCH", "g2g-launch"),
            args,
            output: OutputSpec::RawVideoFile(out),
        })
    }
}
