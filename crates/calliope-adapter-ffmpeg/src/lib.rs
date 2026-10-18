//! ffmpeg adapter. ffmpeg emits framemd5 natively, so it is also the usual
//! reference engine.

use std::path::Path;
use std::process::Command;

use calliope_core::engine::{Engine, EngineInfo, Invocation, OutputSpec, binary, probe_first_line};
use calliope_core::produce::{
    Container, FRAGMENT_DURATION, Produce, ProducedAudio, VideoCodec, whep_endpoint, whip_endpoint,
};
use calliope_core::scenario::{Scenario, Video};
use calliope_core::{Error, Result};

const FRAGMENTED_MP4_FLAGS: &str = "+frag_keyframe+empty_moov+default_base_moof";
const CMAF_MP4_FLAGS: &str = "+cmaf+frag_keyframe+empty_moov+default_base_moof";
const HLS_INIT_SEGMENT: &str = "init.mp4";
// a WHEP reader joins mid-stream and waits for a keyframe, so force one every second
const LIVE_KEYFRAME_EXPRESSION: &str = "expr:gte(t,n_forced)";
// the WHIP muxer refuses B-frames, baseline has none
const LIVE_H264_PROFILE: &str = "baseline";

fn video_encoder(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "libx264",
        VideoCodec::H265 => "libx265",
        VideoCodec::Av1 => "libsvtav1",
    }
}

fn container_args(container: Container, workdir: &Path) -> Vec<String> {
    let fragment = FRAGMENT_DURATION.as_micros().to_string();
    let segment = |ext: &str| workdir.join(format!("seg%05d.{ext}")).display().to_string();
    match container {
        Container::Mp4 => vec!["-f".into(), "mp4".into()],
        Container::Fmp4 => vec![
            "-movflags".into(),
            FRAGMENTED_MP4_FLAGS.into(),
            "-frag_duration".into(),
            fragment,
            "-f".into(),
            "mp4".into(),
        ],
        Container::Cmaf => vec![
            "-movflags".into(),
            CMAF_MP4_FLAGS.into(),
            "-frag_duration".into(),
            fragment,
            "-f".into(),
            "mp4".into(),
        ],
        Container::Mkv => vec!["-f".into(), "matroska".into()],
        Container::Ts => vec!["-f".into(), "mpegts".into()],
        Container::HlsTs => vec![
            "-f".into(),
            "hls".into(),
            "-hls_playlist_type".into(),
            "vod".into(),
            "-hls_segment_type".into(),
            "mpegts".into(),
            "-hls_segment_filename".into(),
            segment("ts"),
        ],
        Container::HlsFmp4 => vec![
            "-f".into(),
            "hls".into(),
            "-hls_playlist_type".into(),
            "vod".into(),
            "-hls_segment_type".into(),
            "fmp4".into(),
            "-hls_fmp4_init_filename".into(),
            HLS_INIT_SEGMENT.into(),
            "-hls_segment_filename".into(),
            segment("m4s"),
        ],
        Container::Dash => vec!["-f".into(), "dash".into()],
        Container::Whip => vec!["-f".into(), "whip".into()],
    }
}

// ffmpeg as the control producer: encode the input with ffmpeg's own encoders and muxers
fn produce_invocation(produce: &Produce, input: &Path, workdir: &Path, engine: &str) -> Invocation {
    let live = produce.container.is_live();
    let mut args: Vec<String> = vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
    ];
    if live {
        args.push("-re".into());
    }
    args.extend(["-i".into(), input.display().to_string()]);
    let encoder = if produce.encode_video {
        video_encoder(produce.video)
    } else {
        "copy"
    };
    args.extend(["-c:v".into(), encoder.into()]);
    if live && produce.encode_video {
        args.extend([
            "-profile:v".into(),
            LIVE_H264_PROFILE.into(),
            "-force_key_frames".into(),
            LIVE_KEYFRAME_EXPRESSION.into(),
        ]);
    }
    match produce.audio {
        None => args.push("-an".into()),
        Some(ProducedAudio::Aac) => args.extend(["-c:a".into(), "copy".into()]),
        Some(ProducedAudio::Opus) => args.extend(["-c:a".into(), "libopus".into()]),
    }
    args.extend(container_args(produce.container, workdir));
    let output = match produce.container.artifact_name() {
        Some(name) => {
            let path = workdir.join(name);
            args.push(path.display().to_string());
            OutputSpec::Produced(path)
        }
        None => {
            args.push(whip_endpoint(engine));
            OutputSpec::Published {
                whep: whep_endpoint(engine),
            }
        }
    };
    Invocation {
        program: binary("CALLIOPE_FFMPEG", "ffmpeg"),
        args,
        output,
    }
}

/// Validate an encode round-trip with ffmpeg: decode the original and the
/// engine's re-encoded elementary stream, then return the PSNR (dB) between them.
/// A high PSNR means the encoder produced a decodable, faithful bitstream; a
/// decode failure or a low PSNR is an encoder bug.
pub fn roundtrip_psnr(
    original: &Path,
    encoded: &Path,
    workdir: &Path,
    video: Video,
) -> Result<f64> {
    let ffmpeg = binary("CALLIOPE_FFMPEG", "ffmpeg");
    let reference = workdir.join("rt_ref.yuv");
    let got = workdir.join("rt_got.yuv");
    decode_to_raw(&ffmpeg, original, &reference)?;
    decode_to_raw(&ffmpeg, encoded, &got)?;
    let size = format!("{}x{}", video.width, video.height);
    let out = Command::new(&ffmpeg)
        .args(["-hide_banner", "-loglevel", "info"])
        .args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-s", &size, "-i"])
        .arg(&got)
        .args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-s", &size, "-i"])
        .arg(&reference)
        .args(["-lavfi", "[0:v][1:v]psnr", "-f", "null", "-"])
        .output()?;
    parse_psnr(&String::from_utf8_lossy(&out.stderr))
        .ok_or_else(|| Error::Parse("ffmpeg reported no PSNR (streams incomparable?)".into()))
}

/// Generate an encode-differential input: ffmpeg encodes a lavfi `source` with
/// `encoder` (plus `extra_args` selecting profiles / features) into an elementary
/// stream at `out`. That stream then feeds the normal differential decode compare,
/// so a decode divergence across engines is a real bug against a hard oracle.
pub fn encode_source(
    source: &str,
    encoder: &str,
    extra_args: &[String],
    video: Video,
    out: &Path,
) -> Result<()> {
    let ffmpeg = binary("CALLIOPE_FFMPEG", "ffmpeg");
    let status = Command::new(&ffmpeg)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", source])
        .args(["-c:v", encoder, "-pix_fmt", video.format.ffmpeg_pix_fmt()])
        .args(extra_args)
        .arg(out)
        .status()?;
    if !status.success() {
        return Err(Error::Parse(format!(
            "ffmpeg could not encode lavfi source '{source}' with {encoder}"
        )));
    }
    Ok(())
}

fn decode_to_raw(ffmpeg: &str, input: &Path, out: &Path) -> Result<()> {
    let status = Command::new(ffmpeg)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(input)
        .args(["-an", "-vf", "format=yuv420p", "-f", "rawvideo"])
        .arg(out)
        .status()?;
    if !status.success() {
        return Err(Error::Parse(format!(
            "ffmpeg could not decode {}",
            input.display()
        )));
    }
    Ok(())
}

/// Pull the `average:<n>` PSNR out of ffmpeg's `psnr` filter log line.
fn parse_psnr(log: &str) -> Option<f64> {
    let rest = &log[log.rfind("average:")? + "average:".len()..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

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
        if let Some(produce) = &scenario.produce {
            return Ok(produce_invocation(produce, input, workdir, self.id()));
        }
        let program = binary("CALLIOPE_FFMPEG", "ffmpeg");
        // audio: decode to normalized interleaved PCM. The decoder is forced to
        // the codec's reference library (`-c:a libopus` for opus) so ffmpeg
        // matches the other libopus-backed engines; the decoder flag must precede
        // `-i`. The whole PCM stream is hashed by the runner.
        if let Some(audio) = scenario.audio {
            let out = workdir.join("out.pcm");
            let mut args = vec!["-nostdin".into(), "-hide_banner".into(), "-y".into()];
            if let Some(dec) = audio.codec.ffmpeg_decoder() {
                args.push("-c:a".into());
                args.push(dec.into());
            }
            args.push("-i".into());
            args.push(input.display().to_string());
            args.extend([
                "-vn".into(),
                "-f".into(),
                audio.format.ffmpeg_fmt().into(),
                "-ar".into(),
                audio.rate.to_string(),
                "-ac".into(),
                audio.channels.to_string(),
                out.display().to_string(),
            ]);
            return Ok(Invocation {
                program,
                args,
                output: OutputSpec::RawAudioFile(out),
            });
        }
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
            // Pin framemd5 to the scenario's target format so the reference
            // hashes the same layout the raw-dump engines convert to. Without
            // this, ffmpeg would hash its native decode (e.g. I420) while the
            // others emit NV12, and every frame would falsely diverge. A
            // deinterlace scenario prepends single-rate yadif, forced on every
            // frame so no engine's interlace detection can disagree.
            let mut filters = Vec::new();
            if scenario.deinterlace {
                filters.push("yadif=mode=send_frame:parity=tff:deint=all".to_string());
            }
            // overlay analog: ffmpeg has no no-op text overlay without fonts,
            // so the closest is the same RGBA colorspace round-trip.
            if scenario.overlay {
                filters.push("format=rgba".to_string());
            }
            if let Some(video) = scenario.video {
                filters.push(format!("format={}", video.format.ffmpeg_pix_fmt()));
            }
            if !filters.is_empty() {
                args.extend(["-vf".into(), filters.join(",")]);
            }
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
