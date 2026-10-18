use std::path::Path;
use std::time::Duration;

use calliope_core::engine::binary;
use calliope_core::produce::{Container, Expectation, Validation, Validator};
use serde::Deserialize;

use crate::{hls, run_tool};

// matroska's default millisecond timestamps are the coarsest any container here stores
const FRAME_INTERVAL_TOLERANCE: Duration = Duration::from_millis(1);
// EXTINF sums to the media duration, within audio priming and frame rounding
const PLAYLIST_DURATION_TOLERANCE_SECONDS: f64 = 0.5;

#[derive(Debug, Deserialize)]
struct Probe {
    streams: Vec<Stream>,
}

#[derive(Debug, Deserialize)]
struct Stream {
    codec_type: String,
    codec_name: String,
    nb_read_frames: Option<String>,
}

// decodes every frame, a demux or decode error lands on stderr at -v error
pub fn validate(artifact: &Path, container: Container, expected: &Expectation) -> Validation {
    let program = binary("CALLIOPE_FFPROBE", "ffprobe");
    let args = [
        "-v",
        "error",
        "-count_frames",
        "-show_entries",
        "stream=codec_type,codec_name,nb_read_frames",
        "-of",
        "json",
    ];
    let output = match run_tool(Validator::Ffprobe, &program, &args, artifact) {
        Ok(output) => output,
        Err(skipped) => return skipped,
    };
    let mut findings = findings(&output.stdout, &output.stderr, expected);
    let timestamps = run_tool(
        Validator::Ffprobe,
        &program,
        &[
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time",
            "-of",
            "csv=p=0",
        ],
        artifact,
    );
    if let Ok(timestamps) = timestamps {
        findings.extend(frame_interval_findings(
            &timestamps.stdout,
            expected.frame_rate,
        ));
    }
    // ffprobe takes an hls duration from the playlist, so check the playlist against the media
    if matches!(container, Container::HlsTs | Container::HlsFmp4) {
        match std::fs::read_to_string(artifact) {
            Ok(playlist) => findings.extend(playlist_findings(&playlist, expected)),
            Err(e) => findings.push(format!("read {}: {e}", artifact.display())),
        }
    }
    Validation::from_findings(Validator::Ffprobe, findings)
}

// the input is constant rate, so every output frame should start one frame interval after the last
fn frame_interval_findings(pts_csv: &str, frame_rate: f64) -> Option<String> {
    let mut pts: Vec<f64> = pts_csv
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    pts.sort_by(f64::total_cmp);
    let interval = 1.0 / frame_rate;
    let irregular: Vec<(usize, f64)> = pts
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .enumerate()
        .filter(|(_, delta)| (delta - interval).abs() > FRAME_INTERVAL_TOLERANCE.as_secs_f64())
        .collect();
    let &(first, delta) = irregular.first()?;
    Some(format!(
        "{} irregular frame intervals, frame {} starts {} us after frame {first} where the input's interval is {} us",
        irregular.len(),
        first + 1,
        Duration::from_secs_f64(delta.max(0.0)).as_micros(),
        Duration::from_secs_f64(interval).as_micros()
    ))
}

fn playlist_findings(playlist: &str, expected: &Expectation) -> Option<String> {
    let listed: f64 = hls::segment_durations(playlist).iter().sum();
    let media = expected.video_frames as f64 / expected.frame_rate;
    ((listed - media).abs() > PLAYLIST_DURATION_TOLERANCE_SECONDS).then(|| {
        format!(
            "the playlist's EXTINF durations sum to {listed:.3} s, the media lasts {media:.3} s"
        )
    })
}

fn findings(stdout: &str, stderr: &str, expected: &Expectation) -> Vec<String> {
    let mut findings: Vec<String> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let probe: Probe = match serde_json::from_str(stdout) {
        Ok(probe) => probe,
        Err(e) => {
            findings.push(format!("unreadable ffprobe report: {e}"));
            return findings;
        }
    };
    let stream = |kind: &str| probe.streams.iter().find(|s| s.codec_type == kind);
    let frames = |s: &Stream| {
        s.nb_read_frames
            .as_deref()
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0)
    };
    match stream("video") {
        None => findings.push("no video stream".into()),
        Some(video) => {
            if video.codec_name != expected.video.ffprobe_name() {
                findings.push(format!(
                    "video codec {} where {} was asked for",
                    video.codec_name,
                    expected.video.ffprobe_name()
                ));
            }
            if frames(video) != expected.video_frames {
                findings.push(format!(
                    "decoded {} video frames, the input has {}",
                    frames(video),
                    expected.video_frames
                ));
            }
        }
    }
    match (stream("audio"), expected.audio) {
        (None, Some(audio)) => findings.push(format!("no {} audio stream", audio.ffprobe_name())),
        (Some(found), Some(audio)) if found.codec_name != audio.ffprobe_name() => {
            findings.push(format!(
                "audio codec {} where {} was asked for",
                found.codec_name,
                audio.ffprobe_name()
            ))
        }
        (Some(found), Some(_)) if frames(found) == 0 => {
            findings.push("the audio stream decodes no frames".into())
        }
        (Some(found), None) => {
            findings.push(format!("unexpected {} audio stream", found.codec_name))
        }
        _ => {}
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use calliope_core::produce::{ProducedAudio, VideoCodec};

    const INPUT_FRAMES: u64 = 300;

    fn expected() -> Expectation {
        Expectation {
            video: VideoCodec::H264,
            audio: Some(ProducedAudio::Aac),
            video_frames: INPUT_FRAMES,
            frame_rate: 25.0,
        }
    }

    #[test]
    fn a_complete_file_has_no_findings() {
        let stdout = include_str!("../testdata/ffprobe-ok.json");
        let stderr = include_str!("../testdata/ffprobe-ok.stderr");
        assert!(findings(stdout, stderr, &expected()).is_empty());
    }

    #[test]
    fn a_truncated_file_reports_decode_errors_and_lost_frames() {
        let stdout = include_str!("../testdata/ffprobe-truncated.json");
        let stderr = include_str!("../testdata/ffprobe-truncated.stderr");
        let findings = findings(stdout, stderr, &expected());
        assert!(findings.iter().any(|f| f.contains("Invalid NAL unit size")));
        assert!(
            findings
                .iter()
                .any(|f| f.contains(&format!("the input has {INPUT_FRAMES}")))
        );
    }

    #[test]
    fn frames_must_keep_the_input_interval() {
        let ffmpeg = include_str!("../testdata/ffprobe-pts-ffmpeg-mp4.csv");
        assert_eq!(frame_interval_findings(ffmpeg, 25.0), None);
        let shifted = include_str!("../testdata/ffprobe-pts-g2g-mp4.csv");
        let finding = frame_interval_findings(shifted, 25.0).unwrap();
        assert!(finding.starts_with("1 irregular"), "{finding}");
        assert!(
            finding.contains("frame 1 starts 33333 us after frame 0"),
            "{finding}"
        );
    }

    #[test]
    fn a_playlist_must_last_as_long_as_its_media() {
        let ffmpeg = include_str!("../testdata/hls-fmp4-ffmpeg.m3u8");
        assert_eq!(playlist_findings(ffmpeg, &expected()), None);
        let doubled = include_str!("../testdata/hls-fmp4-doubled.m3u8");
        assert!(playlist_findings(doubled, &expected()).is_some());
    }

    #[test]
    fn a_wrong_codec_is_a_finding() {
        let stdout = include_str!("../testdata/ffprobe-ok.json");
        let mut expectation = expected();
        expectation.video = VideoCodec::Av1;
        expectation.audio = Some(ProducedAudio::Opus);
        let findings = findings(stdout, "", &expectation);
        assert_eq!(findings.len(), 2, "{findings:?}");
    }
}
