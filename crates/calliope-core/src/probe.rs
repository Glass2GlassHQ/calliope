//! Geometry auto-probe via `ffprobe`, so a differential scenario can omit
//! `[video]` and still hash raw-dump engines. ffprobe reports the decoded
//! width / height / pixel format; the raw-dump engines then convert to that
//! same format (an identity, preserving bit-exactness with ffmpeg's native
//! decode). `CALLIOPE_FFPROBE` overrides the binary.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::corpus::{AudioStream, Media, VideoStream};
use crate::engine::binary;
use crate::scenario::{PixelFormat, Video};
use crate::timestamps::{ReferenceFrame, ReferenceTimestamps, TimeBase};
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StreamKind {
    Video,
    Audio,
}

impl StreamKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }

    // ffprobe's -select_streams for the first stream of this kind
    fn first_stream(self) -> &'static str {
        match self {
            Self::Video => "v:0",
            Self::Audio => "a:0",
        }
    }
}

const FFPROBE_STREAM_ENTRIES: &str = "format=format_name:stream=codec_type,codec_name,width,height,pix_fmt,sample_rate,channels:stream_side_data=side_data_type,crop_top,crop_bottom,crop_left,crop_right";
// the stream entries a geometry probe reads, cropping side data included
const FFPROBE_GEOMETRY_ENTRIES: &str = "stream=width,height,pix_fmt:stream_side_data=side_data_type,crop_top,crop_bottom,crop_left,crop_right";
// ffmpeg crops each decoded picture by this side data, a MOV clean aperture among its sources
const FRAME_CROPPING_SIDE_DATA: &str = "Frame Cropping";

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
    #[serde(default)]
    side_data_list: Vec<ProbeSideData>,
}

#[derive(Debug, Deserialize)]
struct ProbeSideData {
    side_data_type: String,
    crop_top: Option<u32>,
    crop_bottom: Option<u32>,
    crop_left: Option<u32>,
    crop_right: Option<u32>,
}

impl ProbeStream {
    fn video(&self) -> Option<VideoStream> {
        let (width, height) = self.decoded_size()?;
        Some(VideoStream {
            codec: self.codec_name.clone()?,
            width,
            height,
            pix_fmt: self.pix_fmt.clone(),
        })
    }

    fn audio(&self) -> Option<AudioStream> {
        Some(AudioStream {
            codec: self.codec_name.clone()?,
            sample_rate: self.sample_rate.as_deref()?.parse().ok()?,
            channels: self.channels?,
        })
    }

    // the size ffmpeg decodes to: the coded size less any cropping side data
    fn decoded_size(&self) -> Option<(u32, u32)> {
        let (width, height) = (self.width?, self.height?);
        let Some(crop) = self
            .side_data_list
            .iter()
            .find(|data| data.side_data_type == FRAME_CROPPING_SIDE_DATA)
        else {
            return Some((width, height));
        };
        Some((
            width
                .checked_sub(crop.crop_left?)?
                .checked_sub(crop.crop_right?)?,
            height
                .checked_sub(crop.crop_top?)?
                .checked_sub(crop.crop_bottom?)?,
        ))
    }
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
    let video = first_of("video").and_then(ProbeStream::video);
    let audio = first_of("audio").and_then(ProbeStream::audio);
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
            FFPROBE_GEOMETRY_ENTRIES,
            "-of",
            "json",
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
    parse_ffprobe_geometry(&String::from_utf8_lossy(&out.stdout))
}

fn parse_ffprobe_geometry(json: &str) -> Result<Video> {
    let probe: ProbeOutput =
        serde_json::from_str(json).map_err(|e| Error::Parse(format!("ffprobe json: {e}")))?;
    let stream = probe
        .streams
        .first()
        .ok_or_else(|| Error::Parse("ffprobe produced no video stream".into()))?;
    let (width, height) = stream
        .decoded_size()
        .ok_or_else(|| Error::Parse(format!("ffprobe reported no usable size: {stream:?}")))?;
    let pix_fmt = stream
        .pix_fmt
        .as_deref()
        .ok_or_else(|| Error::Parse(format!("ffprobe output missing pix_fmt: {stream:?}")))?;
    Ok(Video {
        width,
        height,
        format: supported_format(pix_fmt)?,
    })
}

fn supported_format(pix_fmt: &str) -> Result<PixelFormat> {
    PixelFormat::from_pix_fmt(pix_fmt).ok_or_else(|| {
        Error::Parse(format!(
            "pix_fmt '{pix_fmt}' is not supported for differential comparison; \
             add an explicit [video] or use a robustness/soak scenario"
        ))
    })
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

// every decoded frame's timestamps from the first stream of `kind`, in decoder output order
// a playlist or manifest input can leave ffprobe waiting on the network, so the probe has a deadline
pub async fn probe_frame_timestamps(
    input: &Path,
    kind: StreamKind,
    timeout: std::time::Duration,
) -> Result<ReferenceTimestamps> {
    let entries = [
        "-select_streams",
        kind.first_stream(),
        "-show_entries",
        "stream=time_base:frame=pts,best_effort_timestamp",
    ];
    parse_frame_timestamps(&ffprobe_json(input, &entries, timeout).await?)
}

// ffprobe's json for `input`, killed at the deadline
async fn ffprobe_json(
    input: &Path,
    entries: &[&str],
    timeout: std::time::Duration,
) -> Result<String> {
    let program = binary("CALLIOPE_FFPROBE", "ffprobe");
    let probe = tokio::process::Command::new(&program)
        .args(["-v", "error"])
        .args(entries)
        .args(["-of", "json"])
        .arg(input)
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(timeout, probe)
        .await
        .map_err(|_| Error::Parse(format!("ffprobe timed out after {} s", timeout.as_secs())))?
        .map_err(|e| Error::Parse(format!("ffprobe ({program}): {e}")))?;
    if !out.status.success() {
        return Err(Error::Parse(format!(
            "ffprobe failed on {}: {}",
            input.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// every video and audio stream in ffprobe's order, None where one cannot be decoded to a known shape
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tracks {
    pub video: Vec<Option<VideoStream>>,
    pub audio: Vec<Option<AudioStream>>,
}

pub async fn probe_tracks(input: &Path, timeout: std::time::Duration) -> Result<Tracks> {
    let entries = ["-show_entries", FFPROBE_STREAM_ENTRIES];
    parse_tracks(&ffprobe_json(input, &entries, timeout).await?)
}

fn parse_tracks(json: &str) -> Result<Tracks> {
    let probe: ProbeOutput =
        serde_json::from_str(json).map_err(|e| Error::Parse(format!("ffprobe json: {e}")))?;
    let of_kind = |kind: &'static str| {
        probe
            .streams
            .iter()
            .filter(move |s| s.codec_type.as_deref() == Some(kind))
    };
    Ok(Tracks {
        video: of_kind("video").map(ProbeStream::video).collect(),
        audio: of_kind("audio").map(ProbeStream::audio).collect(),
    })
}

const MICROSECONDS_PER_SECOND: i128 = 1_000_000;

// the first video stream's keyframes, in whole microseconds from the file's start time, rounded down
pub async fn probe_keyframes(input: &Path, timeout: std::time::Duration) -> Result<Vec<i64>> {
    let entries = [
        "-select_streams",
        StreamKind::Video.first_stream(),
        "-skip_frame",
        "nokey",
        "-show_entries",
        "stream=time_base:format=start_time:frame=key_frame,best_effort_timestamp",
    ];
    parse_keyframes(&ffprobe_json(input, &entries, timeout).await?)
}

#[derive(Debug, Deserialize)]
struct KeyframeProbe {
    #[serde(default)]
    streams: Vec<TimeBaseStream>,
    #[serde(default)]
    frames: Vec<KeyframeEntry>,
    format: Option<StartTime>,
}

#[derive(Debug, Deserialize)]
struct KeyframeEntry {
    key_frame: u8,
    best_effort_timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct StartTime {
    start_time: Option<String>,
}

fn parse_keyframes(json: &str) -> Result<Vec<i64>> {
    let probe: KeyframeProbe =
        serde_json::from_str(json).map_err(|e| Error::Parse(format!("ffprobe json: {e}")))?;
    let stream = probe
        .streams
        .first()
        .ok_or_else(|| Error::Parse("ffprobe found no video stream".into()))?;
    let time_base = TimeBase::parse(&stream.time_base)?;
    let start = match probe.format.and_then(|format| format.start_time) {
        Some(text) => microseconds(&text)?,
        None => 0,
    };
    let to_microseconds = |ticks: i64| {
        (i128::from(ticks) * i128::from(time_base.numerator) * MICROSECONDS_PER_SECOND)
            .div_euclid(i128::from(time_base.denominator))
    };
    probe
        .frames
        .iter()
        .filter(|frame| frame.key_frame == 1)
        .filter_map(|frame| frame.best_effort_timestamp)
        .map(|ticks| {
            i64::try_from(to_microseconds(ticks) - start)
                .map_err(|_| Error::Parse(format!("keyframe at {ticks} ticks is out of range")))
        })
        .collect()
}

// ffprobe prints a start time as seconds with six decimals, exactly the microseconds ffmpeg keeps
fn microseconds(seconds: &str) -> Result<i128> {
    let bad = || Error::Parse(format!("not a start time: {seconds:?}"));
    let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let negative = whole.starts_with('-');
    let whole: i128 = whole.parse().map_err(|_| bad())?;
    if fraction.len() > 6 {
        return Err(bad());
    }
    let fraction: i128 = format!("{fraction:0<6}").parse().map_err(|_| bad())?;
    Ok(whole * MICROSECONDS_PER_SECOND + if negative { -fraction } else { fraction })
}

#[derive(Debug, Deserialize)]
struct FrameProbe {
    #[serde(default)]
    streams: Vec<TimeBaseStream>,
    #[serde(default)]
    frames: Vec<ProbeFrame>,
}

#[derive(Debug, Deserialize)]
struct TimeBaseStream {
    time_base: String,
}

// ffprobe leaves out a field that is N/A
#[derive(Debug, Deserialize)]
struct ProbeFrame {
    pts: Option<i64>,
    best_effort_timestamp: Option<i64>,
}

fn parse_frame_timestamps(json: &str) -> Result<ReferenceTimestamps> {
    let probe: FrameProbe =
        serde_json::from_str(json).map_err(|e| Error::Parse(format!("ffprobe json: {e}")))?;
    let stream = probe
        .streams
        .first()
        .ok_or_else(|| Error::Parse("ffprobe found no stream of that kind".into()))?;
    Ok(ReferenceTimestamps {
        time_base: TimeBase::parse(&stream.time_base)?,
        frames: probe
            .frames
            .into_iter()
            .map(|frame| ReferenceFrame {
                pts: frame.pts,
                best_effort_timestamp: frame.best_effort_timestamp,
            })
            .collect(),
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
    let format = supported_format(next("pix_fmt")?)?;
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

    // ffprobe's report for a DVCPRO HD MOV whose clean aperture ffmpeg 8 crops to
    const CROPPED_STREAM: &str = r#"{"streams": [
        {"codec_name": "dvvideo", "codec_type": "video", "width": 960, "height": 720, "pix_fmt": "yuv422p",
         "side_data_list": [{"side_data_type": "Frame Cropping", "crop_top": 9, "crop_bottom": 9, "crop_left": 12, "crop_right": 12}]}
    ], "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2"}}"#;

    #[test]
    fn probes_report_the_cropped_size_ffmpeg_decodes_to() {
        let video = parse_ffprobe_media(CROPPED_STREAM)
            .unwrap()
            .unwrap()
            .video
            .unwrap();
        assert_eq!((video.width, video.height), (936, 702));
        let geometry = parse_ffprobe_geometry(CROPPED_STREAM).unwrap();
        assert_eq!((geometry.width, geometry.height), (936, 702));
        assert_eq!(geometry.format, PixelFormat::I422);
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
    fn parses_frame_timestamps_with_missing_fields() {
        // ffprobe's report for an AVI-style stream: the second frame has no pts of its own
        let json = r#"{"frames": [
            {"pts": 0, "best_effort_timestamp": 0},
            {"best_effort_timestamp": 1},
            {}
        ], "streams": [{"time_base": "1/25"}]}"#;
        let timestamps = parse_frame_timestamps(json).unwrap();
        assert_eq!(
            timestamps.time_base,
            TimeBase {
                numerator: 1,
                denominator: 25
            }
        );
        let fields: Vec<(Option<i64>, Option<i64>)> = timestamps
            .frames
            .iter()
            .map(|f| (f.pts, f.best_effort_timestamp))
            .collect();
        assert_eq!(fields, [(Some(0), Some(0)), (None, Some(1)), (None, None)]);
        assert!(parse_frame_timestamps(r#"{"frames": []}"#).is_err());
    }

    #[test]
    fn lists_every_track_in_ffprobe_order() {
        // ffprobe's report for gst-medias mp4/multi_audio_5tones.mov, cut to two audio tracks, plus a cover picture
        let json = r#"{"streams": [
            {"codec_name": "h264", "codec_type": "video", "width": 320, "height": 240, "pix_fmt": "yuv420p"},
            {"codec_name": "pcm_s16le", "codec_type": "audio", "sample_rate": "44100", "channels": 1},
            {"codec_type": "video"},
            {"codec_name": "pcm_s16le", "codec_type": "audio", "sample_rate": "48000", "channels": 2}
        ], "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2"}}"#;
        let tracks = parse_tracks(json).unwrap();
        assert_eq!(tracks.video.len(), 2);
        assert!(tracks.video[1].is_none());
        let rates: Vec<Option<u32>> = tracks
            .audio
            .iter()
            .map(|a| a.as_ref().map(|a| a.sample_rate))
            .collect();
        assert_eq!(rates, [Some(44100), Some(48000)]);
    }

    #[test]
    fn keyframes_count_from_the_start_time_in_whole_microseconds() {
        // ffprobe's report for gst-medias bipbop/gear4/fileSequence3.ts, first two keyframes
        let json = r#"{"frames": [
            {"key_frame": 1, "best_effort_timestamp": 3602703},
            {"key_frame": 1, "best_effort_timestamp": 3692793}
        ], "streams": [{"time_base": "1/90000"}], "format": {"start_time": "40.030033"}}"#;
        // 3692793 ticks is 41.031033333 s, one second and 1000 us past the start
        assert_eq!(parse_keyframes(json).unwrap(), [0, 1_001_000]);
        assert_eq!(microseconds("-0.5").unwrap(), -500_000);
        assert!(microseconds("1.1234567").is_err());
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
