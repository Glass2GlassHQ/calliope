use std::collections::{HashMap, VecDeque};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use calliope_adapter_g2g::observe::ChecksumReading;

use super::config::{PipelineConfig, Tier};

// a pts step this much longer than the wall clock step between two frames is a forward jump
pub const PTS_FORWARD_JUMP_NS: i128 = 1_000_000_000;
// a leak shows over a day of one run, a startup ramp or a burst of buffering does not
pub const MEMORY_WINDOW_SECS: u64 = 24 * 3_600;
// 12 MiB a day, about 2 GiB over six months, the whole memory of the canary host
pub const MEMORY_GROWTH_LIMIT_KIB_PER_HOUR: f64 = 512.0;
const SECS_PER_HOUR: f64 = 3_600.0;
const NANOS_PER_MILLI: f64 = 1_000_000.0;
// hls delivers a segment at a time, so a recovered row can go this long without a frame
pub const RECOVERY_TAIL_SECS: f64 = 5.0;

// the digests of a looped clip, by frame index
#[derive(Debug)]
pub struct HashTable {
    // a clip can repeat a frame, so one digest can sit at several indices
    indices: HashMap<String, Vec<usize>>,
    frames: usize,
}

impl HashTable {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parse {}", path.display()))
    }

    // the stdout of `g2g-launch -q ... ! checksumsink`, `<h:mm:ss.nnnnnnnnn> <hex digest>` per frame
    pub fn parse(text: &str) -> Result<Self> {
        let mut indices: HashMap<String, Vec<usize>> = HashMap::new();
        let mut digest_len = None;
        let mut frames = 0;
        for (index, line) in text.lines().enumerate() {
            let line_number = index + 1;
            let Some((pts, digest)) = line.split_once(' ') else {
                bail!("line {line_number}: expected `<pts> <digest>`, got '{line}'");
            };
            ensure!(
                parse_checksum_pts(pts).is_some(),
                "line {line_number}: pts '{pts}' is not h:mm:ss.nnnnnnnnn"
            );
            let lowercase_hex = !digest.is_empty()
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
            ensure!(
                lowercase_hex,
                "line {line_number}: digest '{digest}' is not lowercase hex"
            );
            ensure!(
                *digest_len.get_or_insert(digest.len()) == digest.len(),
                "line {line_number}: digest length differs from the first line's"
            );
            indices.entry(digest.to_string()).or_default().push(index);
            frames += 1;
        }
        ensure!(frames > 0, "no checksum lines");
        Ok(HashTable { indices, frames })
    }
}

// checksumsink's pts form, gst's GST_TIME_FORMAT
fn parse_checksum_pts(text: &str) -> Option<u64> {
    let mut fields = text.split(':');
    let hours: u64 = fields.next()?.parse().ok()?;
    let minutes = fields.next()?;
    let (seconds, nanos) = fields.next()?.split_once('.')?;
    let well_formed = fields.next().is_none()
        && minutes.len() == 2
        && seconds.len() == 2
        && nanos.len() == 9
        && [minutes, seconds, nanos]
            .iter()
            .all(|field| field.bytes().all(|byte| byte.is_ascii_digit()));
    if !well_formed {
        return None;
    }
    let total_seconds =
        hours * 3_600 + minutes.parse::<u64>().ok()? * 60 + seconds.parse::<u64>().ok()?;
    Some(total_seconds * 1_000_000_000 + nanos.parse::<u64>().ok()?)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentCounts {
    // frames whose digest is in the table
    pub matched: u64,
    // frames whose digest is not
    pub mismatched: u64,
    // frames missing between two matched ones
    pub skipped: u64,
}

impl ContentCounts {
    pub fn bad(&self) -> u64 {
        self.mismatched + self.skipped
    }

    // the bad frames that open an incident on a row of this tier
    pub fn faults(&self, tier: Tier) -> u64 {
        match tier {
            // faults break frames on purpose, the recovery check judges these rows instead
            Tier::Faulted => 0,
            Tier::SelfHosted | Tier::Public => self.bad(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameVerdict {
    // not in the table
    Wrong,
    // in the table, with frames missing before it
    AfterSkip,
    // the frame after the previous one
    Next,
}

#[derive(Debug)]
struct ContentCheck {
    table: HashTable,
    previous: Option<usize>,
    counts: ContentCounts,
}

impl ContentCheck {
    fn observe(&mut self, digest: &str) -> FrameVerdict {
        let frames = self.table.frames;
        let expected = self.previous.map(|previous| (previous + 1) % frames);
        let Some(indices) = self.table.indices.get(digest) else {
            self.counts.mismatched += 1;
            // a corrupt frame still takes its slot, so the next good frame is no skip
            self.previous = expected;
            return FrameVerdict::Wrong;
        };
        self.counts.matched += 1;
        let index = expected
            .filter(|expected| indices.contains(expected))
            .unwrap_or(indices[0]);
        let skipped = expected.map_or(0, |expected| ((index + frames - expected) % frames) as u64);
        self.counts.skipped += skipped;
        self.previous = Some(index);
        if skipped == 0 {
            FrameVerdict::Next
        } else {
            FrameVerdict::AfterSkip
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimestampCounts {
    // checksum lines from every sink
    pub frames: u64,
    pub backward: u64,
    pub forward_jumps: u64,
}

impl TimestampCounts {
    pub fn bad(&self) -> u64 {
        self.backward + self.forward_jumps
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct OffsetMean {
    sum_ns: i128,
    frames: i128,
}

impl OffsetMean {
    fn add(&mut self, offset_ns: i128) {
        self.sum_ns += offset_ns;
        self.frames += 1;
    }

    fn take_mean_ns(&mut self) -> Option<f64> {
        let mean = (self.frames > 0).then(|| self.sum_ns as f64 / self.frames as f64);
        *self = OffsetMean::default();
        mean
    }
}

#[derive(Debug)]
struct AvSinks {
    audio_sink: String,
    // arrival minus pts, per branch over the sample interval
    video_offset: OffsetMean,
    audio_offset: OffsetMean,
}

// the current unbroken run of good video frames
#[derive(Debug, Default)]
struct CleanRun {
    // unix seconds the run's first frame arrived, none while frames are bad
    start: Option<f64>,
    last: f64,
    // arrival minus pts over the run
    video_offset: OffsetMean,
    audio_offset: OffsetMean,
}

// how a row came back from a fault
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowRecovery {
    // from the fault's end to the first frame of the run that lasted the clean window, none without such a run
    pub recovery_secs: Option<f64>,
    // over that run, on a row with audio
    pub av_drift_ms: Option<f64>,
}

// what a sample interval's checksum lines showed
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct FrameSample {
    pub content: Option<ContentCounts>,
    pub timestamps: Option<TimestampCounts>,
    // audio arrives this much later than video relative to its pts
    pub av_drift_ms: Option<f64>,
}

#[derive(Debug, Default)]
pub struct FrameChecks {
    video_sink: Option<String>,
    content: Option<ContentCheck>,
    av: Option<AvSinks>,
    // each sink's previous (pts, arrival) in the current run
    previous_frame: HashMap<String, (u64, i128)>,
    timestamps: TimestampCounts,
    clean_run: CleanRun,
}

impl FrameChecks {
    pub fn new(pipeline: &PipelineConfig) -> Result<Self> {
        let content = match &pipeline.expect_hashes {
            Some(path) => Some(ContentCheck {
                table: HashTable::load(path)
                    .with_context(|| format!("pipeline '{}' expect-hashes", pipeline.id))?,
                previous: None,
                counts: ContentCounts::default(),
            }),
            None => None,
        };
        Ok(FrameChecks {
            video_sink: pipeline.video_sink.clone(),
            content,
            av: pipeline.audio_sink.clone().map(|audio_sink| AvSinks {
                audio_sink,
                video_offset: OffsetMean::default(),
                audio_offset: OffsetMean::default(),
            }),
            ..FrameChecks::default()
        })
    }

    // a new run joins the stream anywhere and starts its timestamps afresh
    pub fn restart(&mut self) {
        self.previous_frame.clear();
        self.clean_run = CleanRun::default();
        if let Some(content) = &mut self.content {
            content.previous = None;
        }
    }

    // arrival_ns is the canary's monotonic clock when the line arrived, arrival_unix the wall clock
    pub fn observe(&mut self, reading: &ChecksumReading, arrival_ns: i128, arrival_unix: f64) {
        let pts_ns = i128::from(reading.pts_ns);
        self.timestamps.frames += 1;
        if let Some((previous_pts, previous_arrival)) = self
            .previous_frame
            .insert(reading.element.clone(), (reading.pts_ns, arrival_ns))
        {
            let pts_step = pts_ns - i128::from(previous_pts);
            let wall_step = arrival_ns - previous_arrival;
            if pts_step < 0 {
                self.timestamps.backward += 1;
            } else if pts_step - wall_step > PTS_FORWARD_JUMP_NS {
                self.timestamps.forward_jumps += 1;
            }
        }
        let offset_ns = arrival_ns - pts_ns;
        let from_video = self.video_sink.as_deref() == Some(reading.element.as_str());
        if from_video && let Some(content) = &mut self.content {
            let verdict = content.observe(&reading.digest);
            if verdict != FrameVerdict::Next {
                self.clean_run = CleanRun::default();
            }
            if verdict != FrameVerdict::Wrong {
                let run = &mut self.clean_run;
                run.start.get_or_insert(arrival_unix);
                run.last = arrival_unix;
                run.video_offset.add(offset_ns);
            }
        }
        let Some(av) = &mut self.av else {
            return;
        };
        if from_video {
            av.video_offset.add(offset_ns);
        } else if reading.element == av.audio_sink {
            av.audio_offset.add(offset_ns);
            if self.clean_run.start.is_some() {
                self.clean_run.audio_offset.add(offset_ns);
            }
        }
    }

    // judged at `window_end`, the end of the clean window after a fault that ended at `fault_end`
    pub fn take_recovery(&mut self, fault_end: f64, window_end: f64) -> WindowRecovery {
        let run = &mut self.clean_run;
        let video = run.video_offset.take_mean_ns();
        let audio = run.audio_offset.take_mean_ns();
        let lasted = run.last >= window_end - RECOVERY_TAIL_SECS;
        // a run older than the fault means the fault broke no frame
        let recovery_secs = run
            .start
            .filter(|_| lasted)
            .map(|start| (start - fault_end).max(0.0));
        let av_drift_ms = self
            .av
            .as_ref()
            .and(recovery_secs)
            .and(audio.zip(video))
            .map(|(audio, video)| (audio - video) / NANOS_PER_MILLI);
        WindowRecovery {
            recovery_secs,
            av_drift_ms,
        }
    }

    pub fn take_sample(&mut self) -> FrameSample {
        let timestamps = std::mem::take(&mut self.timestamps);
        let av_drift_ms = self.av.as_mut().and_then(|av| {
            let video = av.video_offset.take_mean_ns();
            let audio = av.audio_offset.take_mean_ns();
            Some((audio? - video?) / NANOS_PER_MILLI)
        });
        FrameSample {
            content: self
                .content
                .as_mut()
                .map(|content| std::mem::take(&mut content.counts)),
            timestamps: (timestamps.frames > 0).then_some(timestamps),
            av_drift_ms,
        }
    }
}

// one run's rss samples over the last MEMORY_WINDOW_SECS
#[derive(Debug, Default)]
pub struct RssWindow {
    pid: Option<u32>,
    run_first_ts: u64,
    points: VecDeque<(u64, u64)>,
}

impl RssWindow {
    // the run's rss growth once it has run for a whole window
    pub fn push(&mut self, pid: u32, ts: u64, rss_kb: u64) -> Option<f64> {
        if self.pid != Some(pid) {
            self.pid = Some(pid);
            self.run_first_ts = ts;
            self.points.clear();
        }
        self.points.push_back((ts, rss_kb));
        let window_start = ts.saturating_sub(MEMORY_WINDOW_SECS);
        while self
            .points
            .front()
            .is_some_and(|&(first, _)| first < window_start)
        {
            self.points.pop_front();
        }
        let whole_window = ts.saturating_sub(self.run_first_ts) >= MEMORY_WINDOW_SECS;
        whole_window.then(|| kib_per_hour(&self.points)).flatten()
    }
}

// least squares slope of rss over time
fn kib_per_hour(points: &VecDeque<(u64, u64)>) -> Option<f64> {
    let count = points.len() as f64;
    let (first_ts, _) = *points.front()?;
    let hours = |ts: u64| (ts - first_ts) as f64 / SECS_PER_HOUR;
    let mean_hours = points.iter().map(|&(ts, _)| hours(ts)).sum::<f64>() / count;
    let mean_kb = points.iter().map(|&(_, kb)| kb as f64).sum::<f64>() / count;
    let (covariance, variance) =
        points
            .iter()
            .fold((0.0, 0.0), |(covariance, variance), &(ts, kb)| {
                let dx = hours(ts) - mean_hours;
                (covariance + dx * (kb as f64 - mean_kb), variance + dx * dx)
            });
    (variance > 0.0).then(|| covariance / variance)
}

#[cfg(test)]
mod tests {
    use super::*;
    use calliope_adapter_g2g::observe::{ObserveMessage, parse_observe_message};

    const FRAME_NS: u64 = 33_333_333;

    // the first four lines of `g2g-launch -q filesrc location=clip.mp4 ! qtdemux ! h264parse ! avdec_h264 ! checksumsink`
    // over a 720p30 testsrc2 clip, standing in for a four frame clip
    const TABLE: &str = "\
0:00:00.000000000 31447f805e0a68a4b1051ebc72ef7613c18930c6
0:00:00.033333333 a189c4fd000d52309f2ccc643bbec1efee42147e
0:00:00.066666667 3061b8ec243e24ec745ca1d24b193013d26af39b
0:00:00.100000000 1128ed923c8017a8abfd5d1ef572117de0b71146
";

    fn digest(index: usize) -> &'static str {
        TABLE.lines().nth(index).unwrap().split_once(' ').unwrap().1
    }

    fn checks(audio: bool) -> FrameChecks {
        FrameChecks {
            video_sink: Some("video".into()),
            content: Some(ContentCheck {
                table: HashTable::parse(TABLE).unwrap(),
                previous: None,
                counts: ContentCounts::default(),
            }),
            av: audio.then(|| AvSinks {
                audio_sink: "audio".into(),
                video_offset: OffsetMean::default(),
                audio_offset: OffsetMean::default(),
            }),
            ..FrameChecks::default()
        }
    }

    #[test]
    fn bad_frames_are_faults_except_on_the_faulted_tier() {
        let bad = ContentCounts {
            matched: 10,
            mismatched: 2,
            skipped: 35,
        };
        assert_eq!(bad.faults(Tier::SelfHosted), 37);
        assert_eq!(bad.faults(Tier::Public), 37);
        assert_eq!(bad.faults(Tier::Faulted), 0);
    }

    const FAULT_END: f64 = 1_792_900_000.0;
    const WINDOW_END: f64 = FAULT_END + 60.0;
    const FRAME_SECS: f64 = FRAME_NS as f64 / 1e9;
    const CORRUPT: &str = "ffff000000000000000000000000000000000000";

    // video frames in clip order from `first_frame`, arriving from `from` on the wall clock, one digest per frame
    fn feed(checks: &mut FrameChecks, digests: &[&str], first_frame: u64, from: f64) {
        for (step, digest) in digests.iter().enumerate() {
            let frame = first_frame + step as u64;
            let arrival = from + step as f64 * FRAME_SECS;
            checks.observe(
                &event("video", frame * FRAME_NS, digest),
                (frame * FRAME_NS) as i128,
                arrival,
            );
        }
    }

    // clip frames for `secs` of playback, starting at clip frame `first`
    fn clip(first: usize, secs: f64) -> Vec<&'static str> {
        let frames = (secs / FRAME_SECS) as usize;
        (first..first + frames)
            .map(|index| digest(index % 4))
            .collect()
    }

    #[test]
    fn recovery_runs_from_the_last_bad_frame_to_the_end_of_the_window() {
        let mut checks = checks(false);
        // corrupt frames until 2 s after the fault, then right frames to the end of the window
        let bad_until = FAULT_END + 2.0;
        feed(&mut checks, &[CORRUPT; 3], 0, bad_until - 3.0 * FRAME_SECS);
        feed(&mut checks, &clip(3, WINDOW_END - bad_until), 3, bad_until);
        let recovery = checks.take_recovery(FAULT_END, WINDOW_END);
        let secs = recovery.recovery_secs.unwrap();
        assert!((secs - 2.0).abs() < 1e-6, "recovered after {secs} s");
        assert_eq!(recovery.av_drift_ms, None, "no audio on this row");
    }

    #[test]
    fn a_skip_breaks_the_run() {
        let mut checks = checks(false);
        feed(&mut checks, &clip(0, 10.0), 0, FAULT_END);
        // two frames go missing 10 s after the fault
        let resumed = FAULT_END + 10.0 + FRAME_SECS;
        feed(&mut checks, &clip(2, WINDOW_END - resumed), 2, resumed);
        let secs = checks
            .take_recovery(FAULT_END, WINDOW_END)
            .recovery_secs
            .unwrap();
        assert!(
            (secs - (resumed - FAULT_END)).abs() < 1e-6,
            "recovered after {secs} s"
        );
    }

    #[test]
    fn a_run_that_stops_before_the_window_ends_did_not_recover() {
        let mut checks = checks(false);
        let stopped = WINDOW_END - RECOVERY_TAIL_SECS - 1.0;
        feed(&mut checks, &clip(0, stopped - FAULT_END), 0, FAULT_END);
        assert_eq!(
            checks.take_recovery(FAULT_END, WINDOW_END).recovery_secs,
            None
        );
    }

    #[test]
    fn a_fault_that_broke_no_frame_recovers_at_once_and_the_run_carries_on() {
        let mut checks = checks(false);
        let earlier = FAULT_END - 90.0;
        feed(&mut checks, &clip(0, WINDOW_END - earlier), 0, earlier);
        assert_eq!(
            checks.take_recovery(FAULT_END, WINDOW_END).recovery_secs,
            Some(0.0)
        );
        // the same unbroken run is still going at the next judgement
        let next_fault_end = WINDOW_END + 30.0;
        let next_window_end = next_fault_end + 60.0;
        let played = ((WINDOW_END - earlier) / FRAME_SECS) as u64;
        feed(
            &mut checks,
            &clip(played as usize, next_window_end - WINDOW_END),
            played,
            WINDOW_END,
        );
        assert_eq!(
            checks
                .take_recovery(next_fault_end, next_window_end)
                .recovery_secs,
            Some(0.0)
        );
    }

    #[test]
    fn a_restart_breaks_the_run() {
        let mut checks = checks(false);
        feed(&mut checks, &clip(0, 20.0), 0, FAULT_END - 10.0);
        checks.restart();
        let rejoined = FAULT_END + 15.0;
        feed(&mut checks, &clip(1, WINDOW_END - rejoined), 1, rejoined);
        let secs = checks
            .take_recovery(FAULT_END, WINDOW_END)
            .recovery_secs
            .unwrap();
        assert!((secs - 15.0).abs() < 1e-6, "recovered after {secs} s");
    }

    #[test]
    fn drift_covers_the_recovered_run_only() {
        let mut checks = checks(true);
        let audio_lag_ns = 40_000_000;
        // before recovery, audio lags far more, and those frames must not count
        let bad_frames = 30;
        for frame in 0..bad_frames {
            let pts = frame * FRAME_NS;
            let arrival = FAULT_END + frame as f64 * FRAME_SECS;
            checks.observe(&event("video", pts, CORRUPT), pts as i128, arrival);
            checks.observe(&event("audio", pts, "1234"), (pts * 10) as i128, arrival);
        }
        let recovered_at = FAULT_END + bad_frames as f64 * FRAME_SECS;
        let frames = ((WINDOW_END - recovered_at) / FRAME_SECS) as u64;
        for step in 0..frames {
            let frame = bad_frames + step;
            let pts = frame * FRAME_NS;
            let arrival = recovered_at + step as f64 * FRAME_SECS;
            checks.observe(
                &event("video", pts, digest(frame as usize % 4)),
                pts as i128,
                arrival,
            );
            checks.observe(
                &event("audio", pts, "1234"),
                pts as i128 + audio_lag_ns,
                arrival,
            );
        }
        let recovery = checks.take_recovery(FAULT_END, WINDOW_END);
        assert!(recovery.recovery_secs.is_some());
        let drift = recovery.av_drift_ms.unwrap();
        assert!((drift - 40.0).abs() < 1e-6, "drift {drift} ms");
    }

    // the line g2g-launch --observe sends for each buffer a named checksumsink digests
    fn event(element: &str, pts_ns: u64, digest: &str) -> ChecksumReading {
        let line = format!(
            r#"{{"digest":"{digest}","element":"{element}","kind":"checksum","pts_ns":{pts_ns},"type":"event"}}"#
        );
        match parse_observe_message(&line) {
            Some(ObserveMessage::Checksum(reading)) => reading,
            other => panic!("expected a checksum event, got {other:?}"),
        }
    }

    #[test]
    fn table_parser_takes_checksumsink_lines_only() {
        let table = HashTable::parse(TABLE).unwrap();
        assert_eq!(table.frames, 4);
        assert_eq!(table.indices[digest(2)], vec![2]);
        assert_eq!(
            parse_checksum_pts("1:02:03.000000004"),
            Some(3_723_000_000_004)
        );
        assert!(HashTable::parse("").is_err());
        assert!(HashTable::parse("Setting pipeline to PLAYING ...\n").is_err());
        assert!(HashTable::parse("0:00:00.0 aaaa\n").is_err());
        assert!(HashTable::parse("0:00:00.000000000 AAAA\n").is_err());
        let mixed_lengths = format!("{TABLE}0:00:00.133333333 abcd\n");
        assert!(HashTable::parse(&mixed_lengths).is_err());
    }

    #[test]
    fn counts_matched_mismatched_and_skipped_frames_across_the_loop() {
        let mut checks = checks(false);
        // join at frame 2, wrap past the clip end, lose frame 1, corrupt frame 3
        let frames = [2, 3, 0, 2];
        for (step, &index) in frames.iter().enumerate() {
            checks.observe(
                &event("video", step as u64 * FRAME_NS, digest(index)),
                step as i128 * FRAME_NS as i128,
                0.0,
            );
        }
        checks.observe(
            &event(
                "video",
                4 * FRAME_NS,
                "ffff000000000000000000000000000000000000",
            ),
            4 * FRAME_NS as i128,
            0.0,
        );
        checks.observe(
            &event("video", 5 * FRAME_NS, digest(0)),
            5 * FRAME_NS as i128,
            0.0,
        );
        let sample = checks.take_sample();
        assert_eq!(
            sample.content,
            Some(ContentCounts {
                matched: 5,
                mismatched: 1,
                skipped: 1,
            })
        );
        assert_eq!(checks.take_sample().content, Some(ContentCounts::default()));
    }

    #[test]
    fn a_restart_rejoins_without_a_skip() {
        let mut checks = checks(false);
        checks.observe(&event("video", 0, digest(0)), 0, 0.0);
        checks.restart();
        checks.observe(&event("video", 0, digest(3)), 1_000_000_000_000, 0.0);
        let sample = checks.take_sample();
        assert_eq!(sample.content.unwrap().skipped, 0);
        assert_eq!(sample.timestamps.unwrap().bad(), 0);
    }

    #[test]
    fn flags_backward_and_far_forward_pts() {
        let mut checks = checks(false);
        // 33-bit mpeg-ts pts wrap, 2^33 ticks of 90 kHz
        let wrap_ns: u64 = (1u64 << 33) * 1_000_000_000 / 90_000;
        let near_wrap = wrap_ns - FRAME_NS;
        let arrivals = [0, 1, 2, 3, 4].map(|step| step * FRAME_NS as i128);
        checks.observe(&event("video", near_wrap, digest(0)), arrivals[0], 0.0);
        // a correct pipeline keeps counting past the wrap
        checks.observe(
            &event("video", near_wrap + FRAME_NS, digest(1)),
            arrivals[1],
            0.0,
        );
        // a broken one wraps back to zero
        checks.observe(&event("video", 0, digest(2)), arrivals[2], 0.0);
        checks.observe(&event("video", FRAME_NS, digest(3)), arrivals[3], 0.0);
        // and one that unwraps twice jumps a whole wrap ahead
        checks.observe(
            &event("video", FRAME_NS + wrap_ns, digest(0)),
            arrivals[4],
            0.0,
        );
        assert_eq!(
            checks.take_sample().timestamps,
            Some(TimestampCounts {
                frames: 5,
                backward: 1,
                forward_jumps: 1,
            })
        );
        assert_eq!(checks.take_sample().timestamps, None);
    }

    #[test]
    fn a_stall_with_matching_wall_time_is_no_jump() {
        let mut checks = checks(false);
        let stall_ns: u64 = 20_000_000_000;
        checks.observe(&event("video", 0, digest(0)), 0, 0.0);
        checks.observe(
            &event("video", stall_ns, digest(1)),
            i128::from(stall_ns),
            0.0,
        );
        // an hls segment arrives in a burst, pts runs ahead of the wall clock frame by frame
        for frame in 2..10 {
            checks.observe(
                &event("video", stall_ns + frame * FRAME_NS, digest(0)),
                i128::from(stall_ns) + 1,
                0.0,
            );
        }
        assert_eq!(checks.take_sample().timestamps.unwrap().bad(), 0);
    }

    #[test]
    fn reports_audio_lag_against_video() {
        let mut checks = checks(true);
        let audio_lag_ns: i128 = 40_000_000;
        for frame in 0..30u64 {
            let pts = frame * FRAME_NS;
            let arrival = i128::from(pts) + 5_000_000;
            checks.observe(
                &event("video", pts, digest(frame as usize % 4)),
                arrival,
                0.0,
            );
            checks.observe(&event("audio", pts, "1234"), arrival + audio_lag_ns, 0.0);
        }
        let sample = checks.take_sample();
        assert_eq!(sample.av_drift_ms, Some(40.0));
        assert_eq!(
            sample.content.unwrap().matched,
            30,
            "audio lines are not content checked"
        );
        checks.observe(&event("video", 0, digest(0)), 0, 0.0);
        assert_eq!(
            checks.take_sample().av_drift_ms,
            None,
            "no audio in the interval"
        );
    }

    #[test]
    fn rss_growth_needs_a_whole_window_of_one_run() {
        let sample_secs = 60;
        let samples_per_window = MEMORY_WINDOW_SECS / sample_secs;
        let leak_kib_per_hour = 1_024;
        let rss_at = |ts: u64| 40_000 + ts * leak_kib_per_hour / 3_600;
        let mut window = RssWindow::default();
        for step in 0..samples_per_window {
            let ts = step * sample_secs;
            assert_eq!(window.push(7, ts, rss_at(ts)), None, "window still filling");
        }
        let full = samples_per_window * sample_secs;
        let growth = window.push(7, full, rss_at(full)).unwrap();
        assert!(
            (growth - leak_kib_per_hour as f64).abs() < 1.0,
            "growth {growth}"
        );
        assert!(growth > MEMORY_GROWTH_LIMIT_KIB_PER_HOUR);
        assert_eq!(
            window.push(8, full + sample_secs, 40_000),
            None,
            "a new run starts over"
        );
    }

    #[test]
    fn flat_rss_with_noise_stays_under_the_limit() {
        let mut window = RssWindow::default();
        let mut growth = None;
        for step in 0..=MEMORY_WINDOW_SECS / 60 {
            // hls buffering swings rss by tens of MB every few minutes
            let swing = if step % 7 < 3 { 45_000 } else { 0 };
            growth = window.push(7, step * 60, 70_000 + swing);
        }
        assert!(growth.unwrap().abs() < MEMORY_GROWTH_LIMIT_KIB_PER_HOUR);
    }
}
