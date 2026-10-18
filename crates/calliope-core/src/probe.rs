//! Geometry auto-probe via `ffprobe`, so a differential scenario can omit
//! `[video]` and still hash raw-dump engines. ffprobe reports the decoded
//! width / height / pixel format; the raw-dump engines then convert to that
//! same format (an identity, preserving bit-exactness with ffmpeg's native
//! decode). `CALLIOPE_FFPROBE` overrides the binary.

use std::path::Path;

use serde::Deserialize;

use crate::corpus::{AudioStream, Media, VideoStream};
use crate::engine::binary;
use crate::scenario::{PixelFormat, Video};
use crate::{Error, Result};

const FFPROBE_STREAM_ENTRIES: &str =
    "format=format_name:stream=codec_type,codec_name,width,height,pix_fmt,sample_rate,channels";

// probe the container and first video / audio stream; None when ffprobe cannot open it as media
pub fn probe_media(input: &Path) -> Result<Option<Media>> {
    let program = binary("CALLIOPE_FFPROBE", "ffprobe");
    let out = std::process::Command::new(&program)
        .args(["-v", "error", "-show_entries", FFPROBE_STREAM_ENTRIES])
        .args(["-of", "json"])
        .arg(input)
        .output()
        .map_err(|e| Error::Parse(format!("ffprobe ({program}): {e}")))?;
    if !out.status.success() {
        return Ok(None);
    }
    parse_ffprobe_media(&String::from_utf8_lossy(&out.stdout))
}

#[derive(Debug, Deserialize)]
struct ProbeOutput {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    format: Option<ProbeFormat>,
}

#[derive(Debug, Deserialize)]
struct ProbeFormat {
    format_name: String,
}

#[derive(Debug, Deserialize)]
struct ProbeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    pix_fmt: Option<String>,
    // ffprobe prints the sample rate as a string
    sample_rate: Option<String>,
    channels: Option<u32>,
}

fn parse_ffprobe_media(json: &str) -> Result<Option<Media>> {
    let probe: ProbeOutput =
        serde_json::from_str(json).map_err(|e| Error::Parse(format!("ffprobe json: {e}")))?;
    let Some(format) = probe.format else {
        return Ok(None);
    };
    let first_of = |kind: &str| {
        probe
            .streams
            .iter()
            .find(|s| s.codec_type.as_deref() == Some(kind) && s.codec_name.is_some())
    };
    let video = first_of("video").and_then(|s| {
        Some(VideoStream {
            codec: s.codec_name.clone()?,
            width: s.width?,
            height: s.height?,
            pix_fmt: s.pix_fmt.clone(),
        })
    });
    let audio = first_of("audio").and_then(|s| {
        Some(AudioStream {
            codec: s.codec_name.clone()?,
            sample_rate: s.sample_rate.as_deref()?.parse().ok()?,
            channels: s.channels?,
        })
    });
    if video.is_none() && audio.is_none() {
        return Ok(None);
    }
    Ok(Some(Media {
        container: format.format_name,
        video,
        audio,
    }))
}

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

// the input's first video stream as a producer sees it: its frame count and rate
#[derive(Debug, Clone, PartialEq)]
pub struct VideoFrameCount {
    pub codec: String,
    pub frames: u64,
    pub frame_rate: f64,
}

pub fn probe_video_frame_count(input: &Path) -> Result<VideoFrameCount> {
    let program = binary("CALLIOPE_FFPROBE", "ffprobe");
    let out = std::process::Command::new(&program)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_packets",
            "-show_entries",
            "stream=codec_name,r_frame_rate,nb_read_packets",
            "-of",
            "default=noprint_wrappers=1",
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
    parse_video_frame_count(&String::from_utf8_lossy(&out.stdout))
}

// ffprobe `key=value` lines for codec_name, r_frame_rate (a ratio) and nb_read_packets
fn parse_video_frame_count(text: &str) -> Result<VideoFrameCount> {
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(key)?.strip_prefix('='))
            .ok_or_else(|| Error::Parse(format!("ffprobe output missing {key}: {text:?}")))
    };
    let bad = |key: &str| Error::Parse(format!("ffprobe {key} not a number: {text:?}"));
    let (numerator, denominator) = field("r_frame_rate")?
        .split_once('/')
        .ok_or_else(|| bad("r_frame_rate"))?;
    let numerator: f64 = numerator.parse().map_err(|_| bad("r_frame_rate"))?;
    let denominator: f64 = denominator.parse().map_err(|_| bad("r_frame_rate"))?;
    Ok(VideoFrameCount {
        codec: field("codec_name")?.to_string(),
        frames: field("nb_read_packets")?
            .parse()
            .map_err(|_| bad("nb_read_packets"))?,
        frame_rate: numerator / denominator,
    })
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
    fn media_probe_keeps_first_video_and_audio_stream() {
        let json = r#"{"streams": [
            {"codec_name": "mov_text", "codec_type": "subtitle"},
            {"codec_name": "h264", "codec_type": "video", "width": 320, "height": 240, "pix_fmt": "yuv420p"},
            {"codec_name": "aac", "codec_type": "audio", "sample_rate": "44100", "channels": 2},
            {"codec_name": "mp3", "codec_type": "audio", "sample_rate": "22050", "channels": 1}
        ], "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2"}}"#;
        let media = parse_ffprobe_media(json).unwrap().unwrap();
        assert_eq!(media.container, "mov,mp4,m4a,3gp,3g2,mj2");
        let video = media.video.unwrap();
        assert_eq!(
            (video.codec.as_str(), video.width, video.height),
            ("h264", 320, 240)
        );
        assert_eq!(video.pix_fmt.as_deref(), Some("yuv420p"));
        let audio = media.audio.unwrap();
        assert_eq!(
            (audio.codec.as_str(), audio.sample_rate, audio.channels),
            ("aac", 44100, 2)
        );
    }

    #[test]
    fn media_probe_drops_files_without_audio_or_video() {
        let subtitles_only = r#"{"streams": [{"codec_name": "subrip", "codec_type": "subtitle"}],
            "format": {"format_name": "srt"}}"#;
        assert!(parse_ffprobe_media(subtitles_only).unwrap().is_none());
        // a stream ffprobe could not name has no decoder to compare
        let unnamed = r#"{"streams": [{"codec_type": "video", "width": 16, "height": 16}],
            "format": {"format_name": "avi"}}"#;
        assert!(parse_ffprobe_media(unnamed).unwrap().is_none());
        assert!(parse_ffprobe_media("{}").unwrap().is_none());
    }

    #[test]
    fn rejects_unsupported_or_malformed() {
        // packed RGB is not bit-exact across engines, so it stays unsupported
        assert!(parse_ffprobe_csv("1920,1080,rgb24").is_err());
        assert!(parse_ffprobe_csv("").is_err());
        assert!(parse_ffprobe_csv("176,,yuv420p").is_err());
        assert!(parse_ffprobe_csv("wide,144,yuv420p").is_err());
    }

    #[test]
    fn parses_video_stream_counts() {
        let v = parse_video_frame_count(
            "codec_name=h264\nr_frame_rate=30000/1001\nnb_read_packets=300\n",
        )
        .unwrap();
        assert_eq!(v.codec, "h264");
        assert_eq!(v.frames, 300);
        assert!((v.frame_rate - 29.97).abs() < 0.01);
        assert!(parse_video_frame_count("codec_name=h264\nr_frame_rate=25/1\n").is_err());
    }
}
