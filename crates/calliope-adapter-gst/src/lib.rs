//! GStreamer adapter: gst-launch-1.0 decodes to a raw I420 dump that the
//! runner hashes framemd5-style.
//!
//! Known caveat: filesink writes buffers as-is, so a decoder that pads
//! strides would break bit-exactness vs ffmpeg's packed layout. Common
//! conformance geometries are unpadded; a videoconvert-to-packed guard is a
//! v2 item.

use std::path::Path;

use calliope_core::Result;
use calliope_core::engine::{Engine, EngineInfo, Invocation, OutputSpec, binary, probe_first_line};
use calliope_core::scenario::Scenario;

#[derive(Debug, Default)]
pub struct GStreamer;

impl Engine for GStreamer {
    fn id(&self) -> &'static str {
        "gstreamer"
    }

    fn probe(&self) -> Result<EngineInfo> {
        probe_first_line(
            self.id(),
            &binary("CALLIOPE_GST_LAUNCH", "gst-launch-1.0"),
            &["--version"],
        )
    }

    fn plan(&self, scenario: &Scenario, input: &Path, workdir: &Path) -> Result<Invocation> {
        let out = workdir.join("out.yuv");
        // convert to the scenario's decoded format so the raw dump matches
        // ffmpeg's native framemd5 layout; a differential run resolves it (from
        // [video] or ffprobe), robustness / soak default to I420.
        let format = scenario.video.map_or("I420", |v| v.format.gst_format());
        let pipeline = format!(
            "filesrc location={} ! decodebin ! videoconvert ! video/x-raw,format={format} ! filesink location={}",
            input.display(),
            out.display()
        );
        let mut args = vec!["-q".to_string()];
        args.extend(pipeline.split(' ').map(str::to_string));
        Ok(Invocation {
            program: binary("CALLIOPE_GST_LAUNCH", "gst-launch-1.0"),
            args,
            output: OutputSpec::RawVideoFile(out),
        })
    }
}
