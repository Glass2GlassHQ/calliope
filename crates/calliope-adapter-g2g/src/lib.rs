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
        Ok(self.invocation(scenario, input, workdir, &[]))
    }

    fn threaded_plan(
        &self,
        scenario: &Scenario,
        input: &Path,
        workdir: &Path,
    ) -> Result<Option<Invocation>> {
        // one OS thread per element; output must stay byte-identical to plan()
        Ok(Some(self.invocation(
            scenario,
            input,
            workdir,
            &["--threads"],
        )))
    }
}

impl G2g {
    /// Build the decode-to-raw invocation, with any leading g2g-launch flags
    /// (e.g. `--threads`) placed before `-q` and the pipeline.
    fn invocation(
        &self,
        scenario: &Scenario,
        input: &Path,
        workdir: &Path,
        flags: &[&str],
    ) -> Invocation {
        let out = workdir.join("out.yuv");
        // Pin the decoded format with a capsfilter (GStreamer fourcc names) so
        // ffmpegdec's Auto output resolves to it and the raw dump matches
        // ffmpeg's framemd5. Planar formats come straight off the decoder;
        // semi-planar NV12 is not a decoder-native output, so insert a
        // videoconvert (an identity repack, still bit-exact) to reach it.
        let format = scenario.video.map_or("I420", |v| v.format.gst_format());
        let convert = match scenario.video {
            Some(v) if !v.format.is_planar_yuv() => "videoconvert ! ",
            _ => "",
        };
        let pipeline = format!(
            "filesrc location={} ! decodebin ! {convert}video/x-raw,format={format} ! filesink location={}",
            input.display(),
            out.display()
        );
        let mut args: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
        args.push("-q".to_string());
        args.extend(pipeline.split(' ').map(str::to_string));
        Invocation {
            program: binary("CALLIOPE_G2G_LAUNCH", "g2g-launch"),
            args,
            output: OutputSpec::RawVideoFile(out),
        }
    }
}
