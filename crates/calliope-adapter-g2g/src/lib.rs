//! glass2glass adapter, driving `g2g-launch` (the gst-launch analog) as a
//! subprocess like every other engine: decode to a raw I420 dump the runner
//! hashes. g2g-launch is not usually on PATH; point CALLIOPE_G2G_LAUNCH at a
//! built binary.

pub mod validate;

use std::path::Path;

use calliope_core::engine::{Engine, EngineInfo, Invocation, OutputSpec, binary, probe_first_line};
use calliope_core::produce::{
    Container, FRAGMENT_DURATION, Produce, ProducedAudio, VideoCodec, whep_endpoint, whip_endpoint,
};
use calliope_core::scenario::Scenario;
use calliope_core::{Error, Result};

const G2G_PLUGIN_FEATURE_RANK_VARIABLE: &str = "G2G_PLUGIN_FEATURE_RANK";
// produce mode reads an mp4 input, decoded h264 when it re-encodes
const PRODUCE_DEMUXER: &str = "qtdemux";
const PRODUCE_INPUT_DECODER: &str = "h264parse ! avdec_h264";
// opus only codes 48 kHz, and opusenc does not negotiate the rate on its own
const OPUS_INPUT_CAPS: &str = "audio/x-raw,format=S16LE,rate=48000";
const HLS_INIT_SEGMENT: &str = "init.mp4";

fn parser(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "h264parse",
        VideoCodec::H265 => "h265parse",
        VideoCodec::Av1 => "av1parse",
    }
}

fn encoder(codec: VideoCodec) -> Result<&'static str> {
    match codec {
        // the default backend is nvenc
        VideoCodec::H264 => Ok("x264enc backend=software"),
        VideoCodec::Av1 => Ok("av1enc"),
        VideoCodec::H265 => Err(Error::Parse("g2g has no H.265 encoder".into())),
    }
}

// the muxer (named `m` when audio fans in) and sink, as one launch fragment
fn mux_and_sink(produce: &Produce, workdir: &Path, engine: &str) -> Result<String> {
    let fragment = FRAGMENT_DURATION.as_millis();
    let fan_in = produce.audio.is_some();
    let name = if fan_in { " name=m" } else { "" };
    let artifact = |name: &str| workdir.join(name).display().to_string();
    // the muxed file, or the playlist for hls
    let primary = || {
        artifact(
            produce
                .container
                .artifact_name()
                .expect("a file container names its artifact"),
        )
    };
    let file_sink = || format!("filesink location={}", primary());
    Ok(match produce.container {
        // only the fan-in mp4mux can write a progressive file
        Container::Mp4 if !fan_in => {
            return Err(Error::Parse(
                "g2g's single-input mp4mux only writes fragmented mp4, give the scenario audio"
                    .into(),
            ));
        }
        Container::Mp4 => format!(
            "mp4mux{name} fragmented=false faststart=true ! {}",
            file_sink()
        ),
        Container::Fmp4 => format!(
            "mp4mux{name} fragment-duration={fragment} ! {}",
            file_sink()
        ),
        Container::Cmaf => format!(
            "mp4mux{name} cmaf=true fragment-duration={fragment} ! {}",
            file_sink()
        ),
        Container::Mkv => format!("matroskamux{name} ! {}", file_sink()),
        Container::Ts => format!("mpegtsmux{name} ! {}", file_sink()),
        Container::HlsTs => format!(
            "mpegtsmux{name} ! hlssink location={} playlist-location={}",
            artifact("seg%05d.ts"),
            primary()
        ),
        Container::HlsFmp4 => format!(
            "mp4mux{name} fragment-duration={fragment} ! hlssink location={} init-location={} playlist-location={}",
            artifact("seg%05d.m4s"),
            artifact(HLS_INIT_SEGMENT),
            primary()
        ),
        Container::Dash => {
            return Err(Error::Parse(
                "g2g has no DASH packager (gst dashsink is unsupported)".into(),
            ));
        }
        Container::Whip => format!("webrtcsink location={}", whip_endpoint(engine)),
    })
}

// g2g-launch's parse errors for input it has no typefind match, demuxer, or decoder for
const UNSUPPORTED_INPUT_ERRORS: [&str; 2] = [
    "unrecognized content, no media type matches",
    "no decoder chain from",
];

// true when a g2g-launch stderr log says the build cannot handle the input at all
pub fn reports_unsupported_input(stderr: &str) -> bool {
    UNSUPPORTED_INPUT_ERRORS
        .iter()
        .any(|error| stderr.contains(error))
}

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
        if let Some(produce) = &scenario.produce {
            return self.produce_invocation(produce, input, workdir);
        }
        if let Some(rt) = &scenario.roundtrip {
            return Ok(self.transcode_invocation(input, workdir, rt));
        }
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
    // demux the mp4 input, re-encode or remux its video, fan its audio into the muxer
    fn produce_invocation(
        &self,
        produce: &Produce,
        input: &Path,
        workdir: &Path,
    ) -> Result<Invocation> {
        // paced to the clock for a live publish, a file source otherwise runs flat out
        let pacing = if produce.container.is_live() {
            "clocksync ! "
        } else {
            ""
        };
        let video = if produce.encode_video {
            format!(
                "{PRODUCE_INPUT_DECODER} ! {pacing}{}",
                encoder(produce.video)?
            )
        } else {
            format!("{}{}", pacing, parser(produce.video))
        };
        let tail = mux_and_sink(produce, workdir, self.id())?;
        let pipeline = match produce.audio {
            None => format!(
                "filesrc location={} ! {PRODUCE_DEMUXER} name=d  d.video_0 ! {video} ! {tail}",
                input.display()
            ),
            Some(audio) => {
                let audio = match audio {
                    ProducedAudio::Aac => "aacparse".to_string(),
                    ProducedAudio::Opus => format!(
                        "aacparse ! avdec_aac ! audioconvert ! audioresample ! {OPUS_INPUT_CAPS} ! opusenc"
                    ),
                };
                format!(
                    "filesrc location={} ! {PRODUCE_DEMUXER} name=d  d.video_0 ! {video} ! m.  d.audio_0 ! {audio} ! m.  {tail}",
                    input.display()
                )
            }
        };
        let output = match produce.container.artifact_name() {
            Some(name) => OutputSpec::Produced(workdir.join(name)),
            None => OutputSpec::Published {
                whep: whep_endpoint(self.id()),
            },
        };
        let mut args = vec!["-q".to_string()];
        args.extend(pipeline.split_whitespace().map(str::to_string));
        Ok(Invocation {
            program: binary("CALLIOPE_G2G_LAUNCH", "g2g-launch"),
            args,
            output,
        })
    }

    /// Build a transcode invocation for an encode round-trip: decode the input
    /// and re-encode it with the named encoder to an elementary stream that
    /// ffmpeg then decodes and PSNR-checks. Exercises g2g's encoders.
    fn transcode_invocation(
        &self,
        input: &Path,
        workdir: &Path,
        rt: &calliope_core::scenario::Roundtrip,
    ) -> Invocation {
        let out = workdir.join(format!("out.{}", rt.output_ext));
        let pipeline = format!(
            "filesrc location={} ! decodebin ! {} ! filesink location={}",
            input.display(),
            rt.encoder,
            out.display()
        );
        let mut args = vec!["-q".to_string()];
        args.extend(pipeline.split(' ').map(str::to_string));
        Invocation {
            program: binary("CALLIOPE_G2G_LAUNCH", "g2g-launch"),
            args,
            output: OutputSpec::EncodedFile(out),
        }
    }

    /// Build the decode-to-PCM invocation for an audio scenario. audioresample
    /// normalizes the rate (identity when it already matches); the whole PCM
    /// stream is hashed by the runner. `dithering=none` because audioconvert
    /// dithers a bit-depth reduction by default (tpdf), which ffmpeg's plain
    /// rounding never matches.
    fn audio_invocation(
        &self,
        audio: &calliope_core::scenario::Audio,
        input: &Path,
        workdir: &Path,
        flags: &[&str],
    ) -> Invocation {
        let out = workdir.join("out.pcm");
        let pipeline = format!(
            "filesrc location={} ! decodebin ! audioconvert dithering=none ! audioresample ! audio/x-raw,format={},rate={},channels={} ! filesink location={}",
            input.display(),
            audio.format.gst_format(),
            audio.rate,
            audio.channels,
            out.display()
        );
        let mut args: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
        args.push("-q".to_string());
        args.extend(pipeline.split(' ').map(str::to_string));
        Invocation {
            program: binary("CALLIOPE_G2G_LAUNCH", "g2g-launch"),
            args,
            output: OutputSpec::RawAudioFile(out),
        }
    }

    /// Build the decode-to-raw invocation, with any leading g2g-launch flags
    /// (e.g. `--threads`) placed before `-q` and the pipeline.
    fn invocation(
        &self,
        scenario: &Scenario,
        input: &Path,
        workdir: &Path,
        flags: &[&str],
    ) -> Invocation {
        if let Some(audio) = &scenario.audio {
            return self.audio_invocation(audio, input, workdir, flags);
        }
        let out = workdir.join("out.yuv");
        // Pin the decoded format with a capsfilter (GStreamer fourcc names) so
        // ffmpegdec's Auto output resolves to it and the raw dump matches
        // ffmpeg's framemd5. Planar formats come straight off the decoder;
        // semi-planar NV12 is not a decoder-native output, so insert a
        // videoconvert (an identity repack, still bit-exact) to reach it.
        let format = scenario.video.map_or("I420", |v| v.format.gst_format());
        // a pinned GPU decoder emits NV12 / P010 whatever the capsfilter asks for
        let pinned = std::env::var_os(G2G_PLUGIN_FEATURE_RANK_VARIABLE).is_some();
        let convert = match scenario.video {
            Some(v) if !v.format.is_planar_yuv() => "videoconvert ! ",
            // a self-compare pins I420 whatever the decoder emits, packed RGB included
            None => "videoconvert ! ",
            _ if pinned => "videoconvert ! ",
            _ => "",
        };
        // deinterlace scenarios insert g2g's yadif; mode=interlaced forces it
        // on every frame like ffmpeg's deint=all, single-rate like fields=top.
        let deint = if scenario.deinterlace {
            "deinterlace method=yadif mode=interlaced ! "
        } else {
            ""
        };
        // overlay scenarios run the decode through a no-cue textoverlay (a
        // strict RGBA-only element) so the demuxed placeholder caps refining
        // mid-stream cross a converter-overlay-converter sandwich; the final
        // capsfilter's convert takes the RGBA back to the dump format.
        let overlay = if scenario.overlay {
            "videoconvert ! textoverlay ! videoconvert ! "
        } else {
            ""
        };
        let pipeline = format!(
            "filesrc location={} ! decodebin ! {deint}{overlay}{convert}video/x-raw,format={format} ! filesink location={}",
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
