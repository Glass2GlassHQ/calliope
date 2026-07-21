//! ffmpeg adapter. ffmpeg emits framemd5 natively, so it is also the usual
//! reference engine.

use std::path::Path;
use std::process::Command;

use calliope_core::engine::{Engine, EngineInfo, Invocation, OutputSpec, binary, probe_first_line};
use calliope_core::scenario::{Scenario, Video};
use calliope_core::{Error, Result};

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
            // others emit NV12, and every frame would falsely diverge.
            if let Some(video) = scenario.video {
                args.extend([
                    "-vf".into(),
                    format!("format={}", video.format.ffmpeg_pix_fmt()),
                ]);
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
