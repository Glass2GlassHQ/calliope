//! GStreamer adapter: gst-launch-1.0 decodes to a raw I420 dump that the
//! runner hashes framemd5-style.
//!
//! `decodebin` is pinned to software decoders (`force-sw-decoders=true`): on a
//! GPU host it otherwise auto-plugs a hardware decoder (e.g. nvh265dec) whose
//! output is not the conformant libav reference, so golden / differential runs
//! diverge non-reproducibly. filesink still writes buffers as-is, so a decoder
//! that pads strides (odd geometries) remains a separate future concern.

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
        // audio: decode to normalized interleaved PCM. audioresample normalizes
        // the rate (identity when it already matches); the whole stream is hashed.
        if let Some(audio) = scenario.audio {
            let out = workdir.join("out.pcm");
            let pipeline = format!(
                "filesrc location={} ! decodebin ! audioconvert ! audioresample ! audio/x-raw,format={},rate={},channels={},layout=interleaved ! filesink location={}",
                input.display(),
                audio.format.gst_format(),
                audio.rate,
                audio.channels,
                out.display()
            );
            let mut args = vec!["-q".to_string()];
            args.extend(pipeline.split(' ').map(str::to_string));
            return Ok(Invocation {
                program: binary("CALLIOPE_GST_LAUNCH", "gst-launch-1.0"),
                args,
                output: OutputSpec::RawAudioFile(out),
            });
        }
        let out = workdir.join("out.yuv");
        // convert to the scenario's decoded format so the raw dump matches
        // ffmpeg's native framemd5 layout; a differential run resolves it (from
        // [video] or ffprobe), robustness / soak default to I420.
        let format = scenario.video.map_or("I420", |v| v.format.gst_format());
        let pipeline = format!(
            "filesrc location={} ! decodebin force-sw-decoders=true ! videoconvert ! video/x-raw,format={format} ! filesink location={}",
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
