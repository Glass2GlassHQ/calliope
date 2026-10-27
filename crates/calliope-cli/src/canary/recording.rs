use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use calliope_core::engine::binary;

use super::config::{PipelineConfig, Tier};
use super::schedule::FaultSchedule;

pub const LOCATION_PLACEHOLDER: &str = "{recording-location}";
pub const MAX_SIZE_TIME_PLACEHOLDER: &str = "{recording-max-size-time}";
// under the log dir
const FAILED_RECORDINGS_DIR: &str = "recordings";
const KEPT_FAILED_FILES: usize = 20;
const FILE_EXTENSION: &str = "mp4";
const NANOS_PER_SEC: u64 = 1_000_000_000;
// a self-hosted stream never pauses, so a longer gap between frames is a lost stretch
pub const SELF_HOSTED_MAX_PTS_GAP_SECS: f64 = 1.0;
// a row that closes no file for this many segment lengths stopped recording
pub const IDLE_SEGMENTS: u64 = 3;
// slack on a file's duration beyond the segment length and one gop
pub const DURATION_SLACK_SECS: f64 = 1.0;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingCounts {
    // closed files checked in the sample interval
    pub checked: u64,
    pub failed: u64,
}

impl RecordingCounts {
    pub fn add(self, other: RecordingCounts) -> RecordingCounts {
        RecordingCounts {
            checked: self.checked + other.checked,
            failed: self.failed + other.failed,
        }
    }
}

pub fn validate(pipeline: &PipelineConfig, recordings_dir: Option<&Path>) -> Result<()> {
    let placeholders = [LOCATION_PLACEHOLDER, MAX_SIZE_TIME_PLACEHOLDER];
    let Some(segment_secs) = pipeline.recording_segment_secs else {
        for placeholder in placeholders {
            ensure!(
                !pipeline.launch.contains(placeholder),
                "{placeholder} needs recording-segment-secs"
            );
        }
        return Ok(());
    };
    ensure!(segment_secs > 0, "recording-segment-secs must be positive");
    // a run's files are named by its start second, and an `ends` row restarts within the same one
    ensure!(!pipeline.ends, "a recording row cannot end");
    ensure!(
        recordings_dir.is_some(),
        "recording-segment-secs needs recordings-dir"
    );
    for placeholder in placeholders {
        ensure!(
            pipeline.launch.matches(placeholder).count() == 1,
            "a recording row's launch line takes {placeholder} once"
        );
    }
    Ok(())
}

fn row_dir(recordings_dir: &Path, pipeline: &PipelineConfig) -> PathBuf {
    recordings_dir.join(&pipeline.id)
}

// each run writes `<run>-<index>.mp4`, since splitmuxsink numbers files from 0 in every run
pub fn launch_line(pipeline: &PipelineConfig, recordings_dir: Option<&Path>, run: u64) -> String {
    let (Some(segment_secs), Some(recordings_dir)) =
        (pipeline.recording_segment_secs, recordings_dir)
    else {
        return pipeline.launch.clone();
    };
    let location = row_dir(recordings_dir, pipeline).join(format!("{run}-%05d.{FILE_EXTENSION}"));
    pipeline
        .launch
        .replace(LOCATION_PLACEHOLDER, &location.to_string_lossy())
        .replace(
            MAX_SIZE_TIME_PLACEHOLDER,
            &(segment_secs * NANOS_PER_SEC).to_string(),
        )
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FileLimits {
    pub segment_secs: f64,
    pub max_pts_gap_secs: f64,
}

impl FileLimits {
    fn new(pipeline: &PipelineConfig, segment_secs: u64, schedule: &FaultSchedule) -> Self {
        let max_pts_gap_secs = match pipeline.tier {
            // a faulted row may see no frame for a whole fault and the clean window it recovers in
            Tier::Faulted => schedule.longest_fault_and_recovery_secs() as f64,
            Tier::SelfHosted | Tier::Public => SELF_HOSTED_MAX_PTS_GAP_SECS,
        };
        FileLimits {
            segment_secs: segment_secs as f64,
            max_pts_gap_secs,
        }
    }
}

// a closed file's verdict, a failed file moved under the log dir
#[derive(Debug, Clone, PartialEq)]
pub struct CheckedFile {
    // relative to the log dir for a failed file
    pub file: String,
    pub failure: Option<String>,
}

#[derive(Debug)]
pub struct RecordingRow {
    id: String,
    dir: PathBuf,
    failed_dir: PathBuf,
    limits: FileLimits,
    segment_secs: u64,
    // unix seconds of the last closed file, or of the canary start
    last_closed: u64,
}

impl RecordingRow {
    // none for a row that does not record
    pub fn new(
        pipeline: &PipelineConfig,
        recordings_dir: Option<&Path>,
        log_dir: &Path,
        schedule: &FaultSchedule,
        now: u64,
    ) -> Result<Option<Self>> {
        let (Some(segment_secs), Some(recordings_dir)) =
            (pipeline.recording_segment_secs, recordings_dir)
        else {
            return Ok(None);
        };
        let dir = row_dir(recordings_dir, pipeline);
        let failed_dir = log_dir.join(FAILED_RECORDINGS_DIR);
        for dir in [&dir, &failed_dir] {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        Ok(Some(RecordingRow {
            id: pipeline.id.clone(),
            dir,
            failed_dir,
            limits: FileLimits::new(pipeline, segment_secs, schedule),
            segment_secs,
            last_closed: now,
        }))
    }

    // checks every closed file once, deleting it when it passes
    pub async fn check_closed_files(&mut self, now: u64) -> Result<Vec<CheckedFile>> {
        let mut checked = Vec::new();
        for (name, path) in self.closed_files()? {
            self.last_closed = now;
            let failure = file_failure(&path, self.limits).await?;
            let file = match &failure {
                None => {
                    std::fs::remove_file(&path)
                        .with_context(|| format!("remove {}", path.display()))?;
                    name
                }
                Some(_) => self.keep_failed(&path, &name, now)?,
            };
            checked.push(CheckedFile { file, failure });
        }
        Ok(checked)
    }

    pub fn idle_failure(&self, now: u64) -> Option<String> {
        let idle_secs = now.saturating_sub(self.last_closed);
        (idle_secs > IDLE_SEGMENTS * self.segment_secs)
            .then(|| format!("no file closed for {idle_secs} s"))
    }

    // a run's newest file is still being written, or was cut off when the run ended
    fn closed_files(&self) -> Result<Vec<(String, PathBuf)>> {
        let mut files = Vec::new();
        for entry in
            std::fs::read_dir(&self.dir).with_context(|| format!("read {}", self.dir.display()))?
        {
            let path = entry
                .with_context(|| format!("read {}", self.dir.display()))?
                .path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .with_context(|| format!("{} is not utf-8", path.display()))?
                .to_string();
            let Some(order) = run_and_index(&name) else {
                bail!(
                    "{} is no `<run>-<index>.{FILE_EXTENSION}` recording",
                    path.display()
                );
            };
            files.push((order, name, path));
        }
        files.sort();
        let latest_run = files.last().map(|((run, _), _, _)| *run);
        let mut closed = Vec::new();
        for (position, ((run, _), name, path)) in files.iter().enumerate() {
            let newest_of_run = files
                .get(position + 1)
                .is_none_or(|((next_run, _), _, _)| next_run != run);
            if !newest_of_run {
                closed.push((name.clone(), path.clone()));
            } else if Some(*run) != latest_run {
                std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
            }
        }
        Ok(closed)
    }

    fn keep_failed(&self, path: &Path, name: &str, now: u64) -> Result<String> {
        // the check time first, so names sort oldest first
        let kept_name = format!("{now}-{}-{name}", self.id);
        let kept = self.failed_dir.join(&kept_name);
        std::fs::rename(path, &kept)
            .with_context(|| format!("move {} to {}", path.display(), kept.display()))?;
        let mut kept_names = Vec::new();
        for entry in std::fs::read_dir(&self.failed_dir)
            .with_context(|| format!("read {}", self.failed_dir.display()))?
        {
            kept_names.push(
                entry
                    .with_context(|| format!("read {}", self.failed_dir.display()))?
                    .path(),
            );
        }
        kept_names.sort();
        let excess = kept_names.len().saturating_sub(KEPT_FAILED_FILES);
        for oldest in &kept_names[..excess] {
            std::fs::remove_file(oldest).with_context(|| format!("remove {}", oldest.display()))?;
        }
        Ok(format!("{FAILED_RECORDINGS_DIR}/{kept_name}"))
    }
}

fn run_and_index(name: &str) -> Option<(u64, u64)> {
    let stem = name.strip_suffix(&format!(".{FILE_EXTENSION}"))?;
    let (run, index) = stem.split_once('-')?;
    Some((run.parse().ok()?, index.parse().ok()?))
}

// ffprobe's view of a file's first video stream
#[derive(Debug, Deserialize)]
struct Probe {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    packets: Vec<ProbePacket>,
    format: Option<ProbeFormat>,
}

#[derive(Debug, Deserialize)]
struct ProbeStream {
    time_base: String,
}

#[derive(Debug, Deserialize)]
struct ProbePacket {
    pts: Option<i64>,
    dts: Option<i64>,
    flags: String,
}

#[derive(Debug, Deserialize)]
struct ProbeFormat {
    // seconds, as a string
    duration: Option<String>,
}

const FFPROBE_ENTRIES: &str = "stream=time_base:packet=pts,dts,flags:format=duration";

// the first check a file fails, none when it passes every one
pub async fn file_failure(path: &Path, limits: FileLimits) -> Result<Option<String>> {
    let ffprobe = binary("CALLIOPE_FFPROBE", "ffprobe");
    let probed = Command::new(&ffprobe)
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", FFPROBE_ENTRIES, "-of", "json"])
        .arg(path)
        .output()
        .await
        .with_context(|| format!("run {ffprobe}"))?;
    if !probed.status.success() {
        return Ok(Some(format!(
            "does not parse: {}",
            first_line(&probed.stderr)
        )));
    }
    let probe: Probe = serde_json::from_slice(&probed.stdout)
        .with_context(|| format!("parse {ffprobe} output for {}", path.display()))?;
    if let Some(failure) = timing_failure(&probe, limits) {
        return Ok(Some(failure));
    }
    let ffmpeg = binary("CALLIOPE_FFMPEG", "ffmpeg");
    let decoded = Command::new(&ffmpeg)
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .await
        .with_context(|| format!("run {ffmpeg}"))?;
    let decode_errors = !decoded.status.success() || !decoded.stderr.trim_ascii().is_empty();
    Ok(decode_errors.then(|| format!("decode error: {}", first_line(&decoded.stderr))))
}

fn first_line(output: &[u8]) -> String {
    String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no message")
        .to_string()
}

fn timing_failure(probe: &Probe, limits: FileLimits) -> Option<String> {
    let Some(stream) = probe.streams.first() else {
        return Some("no video stream".to_string());
    };
    let Some(tick_secs) = parse_time_base(&stream.time_base) else {
        return Some(format!("time base '{}' is not n/d", stream.time_base));
    };
    let secs = |ticks: i64| ticks as f64 * tick_secs;
    let Some(first_packet) = probe.packets.first() else {
        return Some("no video packets".to_string());
    };
    if !first_packet.flags.contains('K') {
        return Some("the first video packet is no keyframe".to_string());
    }
    let mut previous_dts = None;
    let mut keyframe_pts = 0;
    let mut gop_secs: f64 = 0.0;
    let mut all_pts = Vec::with_capacity(probe.packets.len());
    for (index, packet) in probe.packets.iter().enumerate() {
        let (Some(pts), Some(dts)) = (packet.pts, packet.dts) else {
            return Some(format!("packet {index} has no pts or dts"));
        };
        if let Some(previous) = previous_dts
            && dts <= previous
        {
            return Some(format!(
                "dts went from {:.3} s to {:.3} s at packet {index}",
                secs(previous),
                secs(dts)
            ));
        }
        previous_dts = Some(dts);
        if packet.flags.contains('K') {
            if index > 0 {
                gop_secs = gop_secs.max(secs(pts - keyframe_pts));
            }
            keyframe_pts = pts;
        } else if pts < keyframe_pts {
            return Some(format!(
                "pts {:.3} s at packet {index} is below the previous keyframe's {:.3} s",
                secs(pts),
                secs(keyframe_pts)
            ));
        }
        all_pts.push(pts);
    }
    let Some(duration_secs) = probe
        .format
        .as_ref()
        .and_then(|format| format.duration.as_deref()?.parse::<f64>().ok())
    else {
        return Some("no duration".to_string());
    };
    let longest_secs = limits.segment_secs + gop_secs + DURATION_SLACK_SECS;
    if duration_secs > longest_secs {
        return Some(format!(
            "lasts {duration_secs:.3} s, over the {longest_secs:.3} s of a segment, a gop and slack"
        ));
    }
    all_pts.sort_unstable();
    let gap_secs = all_pts
        .windows(2)
        .map(|pair| secs(pair[1] - pair[0]))
        .fold(0.0, f64::max);
    (gap_secs > limits.max_pts_gap_secs).then(|| {
        format!(
            "a {gap_secs:.3} s gap between frames, over the {:.3} s limit",
            limits.max_pts_gap_secs
        )
    })
}

fn parse_time_base(text: &str) -> Option<f64> {
    let (numerator, denominator) = text.split_once('/')?;
    let denominator: f64 = denominator.parse().ok()?;
    (denominator > 0.0).then_some(numerator.parse::<f64>().ok()? / denominator)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canary::schedule::fault_schedule;

    // 30 fps in a 1/15360 time base, the baseline clip's
    const TIME_BASE: &str = "1/15360";
    const FRAME_TICKS: i64 = 512;
    const GOP_FRAMES: i64 = 30;
    const SECS: f64 = 1.0 / 15360.0;
    const LIMITS: FileLimits = FileLimits {
        segment_secs: 2.0,
        max_pts_gap_secs: SELF_HOSTED_MAX_PTS_GAP_SECS,
    };

    fn packet(frame: i64) -> ProbePacket {
        let keyframe = frame % GOP_FRAMES == 0;
        ProbePacket {
            pts: Some(frame * FRAME_TICKS),
            dts: Some(frame * FRAME_TICKS),
            flags: if keyframe { "K__" } else { "___" }.to_string(),
        }
    }

    fn probe(packets: Vec<ProbePacket>) -> Probe {
        let last_pts = packets.iter().filter_map(|packet| packet.pts).max();
        let duration = last_pts.map(|pts| ((pts + FRAME_TICKS) as f64 * SECS).to_string());
        Probe {
            streams: vec![ProbeStream {
                time_base: TIME_BASE.to_string(),
            }],
            packets,
            format: Some(ProbeFormat { duration }),
        }
    }

    // two gops, as long as the segment
    fn two_gops() -> Vec<ProbePacket> {
        (0..2 * GOP_FRAMES).map(packet).collect()
    }

    fn assert_fails(probe: &Probe, limits: FileLimits, expected: &str) {
        let failure = timing_failure(probe, limits);
        assert!(
            failure
                .as_deref()
                .is_some_and(|failure| failure.contains(expected)),
            "expected a failure containing '{expected}', got {failure:?}"
        );
    }

    #[test]
    fn a_clean_file_passes_the_timing_checks() {
        assert_eq!(timing_failure(&probe(two_gops()), LIMITS), None);
    }

    #[test]
    fn a_file_without_video_fails() {
        let mut no_stream = probe(two_gops());
        no_stream.streams.clear();
        assert_fails(&no_stream, LIMITS, "no video stream");
        assert_fails(&probe(Vec::new()), LIMITS, "no video packets");
    }

    #[test]
    fn a_file_must_start_on_a_keyframe() {
        let packets = (1..2 * GOP_FRAMES).map(packet).collect();
        assert_fails(&probe(packets), LIMITS, "no keyframe");
    }

    #[test]
    fn dts_must_rise() {
        let mut packets = two_gops();
        packets[10].dts = packets[9].dts;
        assert_fails(&probe(packets), LIMITS, "dts went from");
    }

    #[test]
    fn no_frame_may_come_before_its_keyframe() {
        let mut packets = two_gops();
        // dts still rises, the pts of the frame after the second keyframe drops behind it
        let after_keyframe = GOP_FRAMES as usize + 1;
        packets[after_keyframe].pts = Some((GOP_FRAMES - 2) * FRAME_TICKS);
        assert_fails(&probe(packets), LIMITS, "below the previous keyframe");
    }

    #[test]
    fn a_file_may_last_a_segment_a_gop_and_slack() {
        // a segment, one gop past it to the next keyframe, and a frame
        let packets = (0..3 * GOP_FRAMES + 1).map(packet).collect();
        let short_segment = FileLimits {
            segment_secs: 1.0,
            ..LIMITS
        };
        assert_eq!(timing_failure(&probe(packets), LIMITS), None);
        let packets = (0..3 * GOP_FRAMES + 1).map(packet).collect();
        assert_fails(&probe(packets), short_segment, "over the");
    }

    #[test]
    fn a_gap_between_frames_fails_past_the_row_limit() {
        let mut packets = two_gops();
        let gap_ticks = 2 * GOP_FRAMES * FRAME_TICKS;
        for packet in &mut packets[GOP_FRAMES as usize..] {
            packet.pts = packet.pts.map(|pts| pts + gap_ticks);
            packet.dts = packet.dts.map(|dts| dts + gap_ticks);
        }
        let gap = probe(packets);
        assert_fails(&gap, LIMITS, "gap between frames");
        let faulted = FileLimits {
            max_pts_gap_secs: fault_schedule().longest_fault_and_recovery_secs() as f64,
            ..LIMITS
        };
        assert_eq!(timing_failure(&gap, faulted), None);
    }

    fn pipeline(launch: &str, recording_segment_secs: Option<u64>) -> PipelineConfig {
        PipelineConfig {
            id: "baseline-record".into(),
            tier: Tier::SelfHosted,
            launch: launch.into(),
            ends: false,
            note: None,
            video_sink: None,
            audio_sink: None,
            expect_hashes: None,
            recording_segment_secs,
            checked_by: None,
        }
    }

    const RECORDING_LAUNCH: &str = "rtspsrc location=rtsp://127.0.0.1:18554/baseline ! h264parse ! splitmuxsink location={recording-location} max-size-time={recording-max-size-time}";

    #[test]
    fn the_canary_fills_in_each_run_location_and_the_segment_length() {
        let recordings = Path::new("recordings");
        let recording = pipeline(RECORDING_LAUNCH, Some(60));
        validate(&recording, Some(recordings)).unwrap();
        assert_eq!(
            launch_line(&recording, Some(recordings), 1_792_900_000),
            "rtspsrc location=rtsp://127.0.0.1:18554/baseline ! h264parse ! splitmuxsink location=recordings/baseline-record/1792900000-%05d.mp4 max-size-time=60000000000"
        );
        assert!(validate(&recording, None).is_err());
        assert!(validate(&pipeline(RECORDING_LAUNCH, None), Some(recordings)).is_err());
        let no_location =
            RECORDING_LAUNCH.replace("location={recording-location}", "location=x%05d.mp4");
        assert!(validate(&pipeline(&no_location, Some(60)), Some(recordings)).is_err());
        assert!(validate(&pipeline(RECORDING_LAUNCH, Some(0)), Some(recordings)).is_err());
        let ends = PipelineConfig {
            ends: true,
            ..recording.clone()
        };
        assert!(validate(&ends, Some(recordings)).is_err());
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "calliope-canary-recording-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn row(scratch: &Path, now: u64) -> RecordingRow {
        RecordingRow::new(
            &pipeline(RECORDING_LAUNCH, Some(60)),
            Some(&scratch.join("recordings")),
            &scratch.join("log"),
            &fault_schedule(),
            now,
        )
        .unwrap()
        .unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn each_runs_newest_file_is_open_and_a_dead_runs_is_dropped() {
        let dir = scratch("closed");
        let row = row(&dir, 0);
        for name in ["100-00000.mp4", "100-00001.mp4", "100-00002.mp4"] {
            std::fs::write(row.dir.join(name), b"").unwrap();
        }
        let closed: Vec<String> = row
            .closed_files()
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(closed, ["100-00000.mp4", "100-00001.mp4"]);

        // a later run makes run 100's newest file one that never closed
        std::fs::write(row.dir.join("250-00000.mp4"), b"").unwrap();
        let closed: Vec<String> = row
            .closed_files()
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(closed, ["100-00000.mp4", "100-00001.mp4"]);
        assert_eq!(
            names(&row.dir),
            ["100-00000.mp4", "100-00001.mp4", "250-00000.mp4"]
        );

        std::fs::write(row.dir.join("stray.txt"), b"").unwrap();
        assert!(row.closed_files().is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keeps_the_newest_failed_files() {
        let dir = scratch("kept");
        let row = row(&dir, 0);
        for now in 0..KEPT_FAILED_FILES as u64 + 2 {
            let path = row.dir.join("100-00000.mp4");
            std::fs::write(&path, b"").unwrap();
            let kept = row
                .keep_failed(&path, "100-00000.mp4", 1_792_900_000 + now)
                .unwrap();
            assert_eq!(
                kept,
                format!(
                    "recordings/{}-baseline-record-100-00000.mp4",
                    1_792_900_000 + now
                )
            );
        }
        let kept = names(&row.failed_dir);
        assert_eq!(kept.len(), KEPT_FAILED_FILES);
        assert!(kept[0].starts_with("1792900002-"), "{kept:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn idle_after_three_segments_without_a_closed_file() {
        let dir = scratch("idle");
        let row = row(&dir, 1_000);
        assert_eq!(row.idle_failure(1_000 + IDLE_SEGMENTS * 60), None);
        assert!(row.idle_failure(1_001 + IDLE_SEGMENTS * 60).is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // runs ffmpeg to generate the files and ffprobe and ffmpeg to check them, ci-smoke.sh runs these
    fn ffmpeg(args: &[&str]) {
        let ffmpeg = binary("CALLIOPE_FFMPEG", "ffmpeg");
        let status = std::process::Command::new(&ffmpeg)
            .args(["-nostdin", "-v", "error", "-y"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "{ffmpeg} {args:?}");
    }

    // three one-second gops at 30 fps, without b-frames
    const ENCODE: [&str; 8] = [
        "-c:v", "libx264", "-g", "30", "-bf", "0", "-pix_fmt", "yuv420p",
    ];
    const SOURCE: [&str; 6] = [
        "-f",
        "lavfi",
        "-t",
        "3",
        "-i",
        "testsrc2=size=160x120:rate=30",
    ];

    fn encode(path: &Path, filters: &[&str]) {
        let mut args: Vec<&str> = SOURCE.to_vec();
        args.extend_from_slice(filters);
        args.extend_from_slice(&ENCODE);
        args.push(path.to_str().unwrap());
        ffmpeg(&args);
    }

    async fn failure(path: &Path, limits: FileLimits) -> Option<String> {
        file_failure(path, limits).await.unwrap()
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg with libx264, and ffprobe"]
    async fn generated_files_pass_or_fail_each_check() {
        let dir = scratch("files");
        let good = dir.join("good.mp4");
        encode(&good, &[]);
        assert_eq!(failure(&good, LIMITS).await, None);

        let segment_shorter_than_the_file = FileLimits {
            segment_secs: 0.5,
            ..LIMITS
        };
        let too_long = failure(&good, segment_shorter_than_the_file).await;
        assert!(
            too_long.as_deref().is_some_and(|f| f.contains("lasts")),
            "{too_long:?}"
        );

        let truncated = dir.join("truncated.mp4");
        let bytes = std::fs::read(&good).unwrap();
        std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();
        let unparsed = failure(&truncated, LIMITS).await;
        assert!(
            unparsed
                .as_deref()
                .is_some_and(|f| f.contains("does not parse")),
            "{unparsed:?}"
        );

        // keeps the frames before the first keyframe after half a second
        let no_keyframe_first = dir.join("no-keyframe-first.mp4");
        let good_path = good.to_str().unwrap();
        ffmpeg(&[
            "-i",
            good_path,
            "-c",
            "copy",
            "-ss",
            "0.5",
            "-copyinkf",
            no_keyframe_first.to_str().unwrap(),
        ]);
        let no_keyframe = failure(&no_keyframe_first, LIMITS).await;
        assert!(
            no_keyframe
                .as_deref()
                .is_some_and(|f| f.contains("no keyframe")),
            "{no_keyframe:?}"
        );

        // two seconds missing after the first gop
        let gap = dir.join("gap.mp4");
        encode(
            &gap,
            &[
                "-vf",
                "setpts=PTS+gte(N\\,30)*2/TB",
                "-fps_mode",
                "passthrough",
            ],
        );
        let gapped = failure(&gap, LIMITS).await;
        assert!(
            gapped
                .as_deref()
                .is_some_and(|f| f.contains("gap between frames")),
            "{gapped:?}"
        );

        let corrupt = dir.join("corrupt.mp4");
        let mut bytes = bytes.clone();
        let third = bytes.len() / 3;
        for byte in &mut bytes[third..third + 400] {
            *byte ^= 0x55;
        }
        std::fs::write(&corrupt, &bytes).unwrap();
        let undecodable = failure(&corrupt, LIMITS).await;
        assert!(
            undecodable
                .as_deref()
                .is_some_and(|f| f.contains("decode error")),
            "{undecodable:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
