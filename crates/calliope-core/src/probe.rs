//! Geometry auto-probe via `ffprobe`, so a differential scenario can omit
//! `[video]` and still hash raw-dump engines. ffprobe reports the decoded
//! width / height / pixel format; the raw-dump engines then convert to that
//! same format (an identity, preserving bit-exactness with ffmpeg's native
//! decode). `CALLIOPE_FFPROBE` overrides the binary.

use std::path::Path;

use crate::engine::binary;
use crate::scenario::{PixelFormat, Video};
use crate::{Error, Result};

/// probe the first video stream of `input` for its decoded geometry
pub fn probe_geometry(input: &Path) -> Result<Video> {
    let program = binary("CALLIOPE_FFPROBE", "ffprobe");
    let out = std::process::Command::new(&program)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,pix_fmt",
            "-of",
            "csv=p=0",
        ])
        .arg(input)
        .output()
        .map_err(|e| Error::Parse(format!("ffprobe ({program}): {e}")))?;
    if !out.status.success() {
        return Err(Error::Parse(format!(
            "ffprobe failed on {}: {}",
            input.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    parse_ffprobe_csv(&String::from_utf8_lossy(&out.stdout))
}

/// Probe every decoded frame's geometry (a resolution-changing stream yields
/// several distinct sizes). The resolution-change oracle sums the per-frame
/// packed sizes to get the expected decoded byte total. Decodes the whole
/// stream, so keep such vectors small.
pub fn probe_frame_geometry(input: &Path) -> Result<Vec<Video>> {
    let program = binary("CALLIOPE_FFPROBE", "ffprobe");
    let out = std::process::Command::new(&program)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=width,height,pix_fmt",
            "-of",
            "csv=p=0",
        ])
        .arg(input)
        .output()
        .map_err(|e| Error::Parse(format!("ffprobe ({program}): {e}")))?;
    if !out.status.success() {
        return Err(Error::Parse(format!(
            "ffprobe failed on {}: {}",
            input.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let frames = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(parse_ffprobe_csv)
        .collect::<Result<Vec<_>>>()?;
    if frames.is_empty() {
        return Err(Error::Parse("ffprobe reported no frames".into()));
    }
    Ok(frames)
}

/// parse ffprobe `-of csv=p=0` output for `width,height,pix_fmt` (a single
/// stream or frame line; any trailing fields, e.g. per-frame side data, ignored)
fn parse_ffprobe_csv(text: &str) -> Result<Video> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .ok_or_else(|| Error::Parse("ffprobe produced no video stream".into()))?;
    let mut fields = line.split(',');
    let mut next = |what: &str| {
        fields
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Parse(format!("ffprobe output missing {what}: {line:?}")))
    };
    let width = next("width")?
        .parse()
        .map_err(|_| Error::Parse(format!("ffprobe width not a number: {line:?}")))?;
    let height = next("height")?
        .parse()
        .map_err(|_| Error::Parse(format!("ffprobe height not a number: {line:?}")))?;
    let pix_fmt = next("pix_fmt")?;
    let format = PixelFormat::from_pix_fmt(pix_fmt).ok_or_else(|| {
        Error::Parse(format!(
            "pix_fmt '{pix_fmt}' is not supported for differential comparison; \
             add an explicit [video] or use a robustness/soak scenario"
        ))
    })?;
    Ok(Video {
        width,
        height,
        format,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_probe_output() {
        let v = parse_ffprobe_csv("176,144,yuv420p\n").unwrap();
        assert_eq!((v.width, v.height, v.format), (176, 144, PixelFormat::I420));

        let v = parse_ffprobe_csv("1920,1080,yuv422p").unwrap();
        assert_eq!(
            (v.width, v.height, v.format),
            (1920, 1080, PixelFormat::I422)
        );
    }

    #[test]
    fn probes_nv12_and_high_bit_depth() {
        let v = parse_ffprobe_csv("1920,1080,yuv420p10le").unwrap();
        assert_eq!(
            (v.width, v.height, v.format),
            (1920, 1080, PixelFormat::I420p10)
        );
        let v = parse_ffprobe_csv("176,144,nv12").unwrap();
        assert_eq!((v.width, v.height, v.format), (176, 144, PixelFormat::Nv12));
    }

    #[test]
    fn rejects_unsupported_or_malformed() {
        // packed RGB is not bit-exact across engines, so it stays unsupported
        assert!(parse_ffprobe_csv("1920,1080,rgb24").is_err());
        assert!(parse_ffprobe_csv("").is_err());
        assert!(parse_ffprobe_csv("176,,yuv420p").is_err());
        assert!(parse_ffprobe_csv("wide,144,yuv420p").is_err());
    }
}
