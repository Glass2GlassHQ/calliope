// real-world corpus sweep: one synthesized scenario per decodable stream of every imported sample

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use calliope_core::corpus::{self, Manifest, Vector};
use calliope_core::framehash::parse_checksum_lines;
use calliope_core::probe::{StreamKind, probe_frame_timestamps};
use calliope_core::report::ScenarioReport;
use calliope_core::runner::{RunResult, RunStatus, STDOUT_LOG};
use calliope_core::scenario::{
    Audio, AudioCodec, Determinism, Input, PixelFormat, SampleFormat, Scenario, Video,
};
use calliope_core::timestamps::{
    self, FramePairing, ReferenceTimestamps, TimeBase, TimestampVerdict,
};

pub const REFERENCE_ENGINE: &str = "ffmpeg";
const GSTREAMER_ENGINE: &str = "gstreamer";
const G2G_ENGINE: &str = "g2g";
// real-world samples are short clips, so a minute means a hang
const SWEEP_TIMEOUT_SECS: u64 = 60;
// determinism repeats each engine just enough to catch a run-to-run difference
const DETERMINISM_RUNS: usize = 2;
// scenarios per run_matrix batch, decoded output is deleted after each batch
const SWEEP_BATCH: usize = 8;
const REPRODUCER_DIR: &str = "/tmp/calliope-repro";
const RUN_LOG_EXTENSION: &str = "log";
// table placeholder for a video stream ffprobe gave no pixel format
const NO_PIX_FMT: &str = "none";
// ffprobe's codec_name prefix for every raw PCM sample format
const PCM_CODEC_PREFIX: &str = "pcm_";

// filesrc block sizes the chunk-size mode decodes at, the first is g2g's default and the baseline
const CHUNK_SIZES: [u32; 3] = [65536, 4096, 188];

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SweepMode {
    // every engine decodes each stream, ffmpeg is the reference
    Decode,
    // g2g's per-frame pts against ffprobe -show_frames
    Timestamps,
    // g2g decodes at each of CHUNK_SIZES and the outputs must be identical
    ChunkSize,
}

#[derive(Debug)]
pub struct SweepCase {
    pub vector: String,
    pub kind: StreamKind,
    // codec plus pix_fmt for video, as shown in the triage table
    pub codec: String,
    pub scenario: Scenario,
}

// bit-exact video goes differential, everything else self-compares
pub fn sweep_cases(vectors: &[Vector], engines: &[String]) -> Vec<SweepCase> {
    let mut cases = Vec::new();
    for vector in vectors {
        // the gst-launch lines the adapters build split on spaces
        if vector.id.contains(char::is_whitespace) {
            continue;
        }
        let Some(media) = &vector.media else {
            continue;
        };
        if let Some(video) = &media.video {
            let pix_fmt = video.pix_fmt.as_deref().unwrap_or(NO_PIX_FMT);
            let mut scenario = base_scenario(vector, StreamKind::Video, engines.to_vec());
            match video.pix_fmt.as_deref().and_then(PixelFormat::from_pix_fmt) {
                Some(format) => {
                    scenario.video = Some(Video {
                        width: video.width,
                        height: video.height,
                        format,
                    });
                }
                None => scenario.determinism = Some(determinism()),
            }
            cases.push(SweepCase {
                vector: vector.id.clone(),
                kind: StreamKind::Video,
                codec: format!("{} {pix_fmt}", video.codec),
                scenario,
            });
        }
        if let Some(audio) = &media.audio {
            let codec = AudioCodec::from_codec_name(&audio.codec);
            let engines: Vec<String> = engines
                .iter()
                .filter(|e| !(codec.needs_shared_libavcodec() && *e == GSTREAMER_ENGINE))
                .cloned()
                .collect();
            let mut scenario = base_scenario(vector, StreamKind::Audio, engines);
            scenario.audio = Some(Audio {
                codec,
                rate: audio.sample_rate,
                channels: audio.channels,
                format: SampleFormat::S16le,
            });
            if !codec.differential_eligible() {
                scenario.determinism = Some(determinism());
            }
            cases.push(SweepCase {
                vector: vector.id.clone(),
                kind: StreamKind::Audio,
                codec: audio.codec.clone(),
                scenario,
            });
        }
    }
    cases
}

fn determinism() -> Determinism {
    Determinism {
        runs: DETERMINISM_RUNS,
        threads: false,
    }
}

fn base_scenario(vector: &Vector, kind: StreamKind, engines: Vec<String>) -> Scenario {
    Scenario {
        id: format!("{}/{}", vector.id, kind.label()),
        engines,
        reference: REFERENCE_ENGINE.into(),
        timeout_secs: SWEEP_TIMEOUT_SECS,
        input: Input {
            corpus: Some(vector.id.clone()),
            path: None,
        },
        video: None,
        audio: None,
        fault: None,
        soak: None,
        produce: None,
        determinism: None,
        golden: false,
        roundtrip: None,
        encode: None,
        resolution_change: false,
        deinterlace: false,
        overlay: false,
        blocksize: None,
        timestamps: false,
        outcome_diff: false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TriageClass {
    G2gCrashOrHang,
    G2gDiverges,
    G2gNoOutput,
    GstreamerDiverges,
    ReferenceQuirk,
    UnsupportedCodec,
    Agree,
}

impl TriageClass {
    fn title(self) -> &'static str {
        match self {
            Self::G2gCrashOrHang => "g2g crash / hang",
            Self::G2gDiverges => "g2g diverges",
            Self::G2gNoOutput => "g2g no output",
            Self::GstreamerDiverges => "gstreamer diverges",
            Self::ReferenceQuirk => "reference quirk",
            Self::UnsupportedCodec => "unsupported codec",
            Self::Agree => "agree",
        }
    }
}

// one engine's result reduced to what triage compares
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Decoded,
    // wrote output the hashing could not split into whole frames
    Unhashable,
    NoOutput,
    Crashed,
    Hung,
}

fn outcome(run: &RunResult) -> Outcome {
    match (&run.status, run.decoded) {
        (RunStatus::Signaled { .. }, _) => Outcome::Crashed,
        (RunStatus::Timeout, _) => Outcome::Hung,
        (RunStatus::Ok, Some(true)) => Outcome::Decoded,
        (RunStatus::Error { .. }, Some(true)) => Outcome::Unhashable,
        _ => Outcome::NoOutput,
    }
}

fn status_note(run: &RunResult) -> String {
    match &run.status {
        RunStatus::Ok => "no output".into(),
        RunStatus::ExitFailure { code } => format!("exit {code}"),
        RunStatus::Signaled { signal } => format!("signal {signal}"),
        RunStatus::Timeout => format!("timeout after {SWEEP_TIMEOUT_SECS}s"),
        RunStatus::Error { message } => message.clone(),
    }
}

// a candidate's disagreement with the reference, None when it agrees
fn divergence(report: &ScenarioReport, run: &RunResult) -> Option<String> {
    if report.determinism {
        return (run.determinism_matched != Some(true)).then(|| "nondeterministic output".into());
    }
    let entry = report.runs.iter().find(|r| r.run.engine == run.engine)?;
    let comparison = entry.comparison.as_ref()?;
    if comparison.matched {
        return None;
    }
    Some(match comparison.first_divergence {
        Some(frame) => format!(
            "diverged at frame {frame} of {}",
            comparison.reference_frames
        ),
        None => format!(
            "{} frames vs reference {}",
            comparison.frames, comparison.reference_frames
        ),
    })
}

// the first line of an engine log that reports an error, skipping libav's bracketed decoder chatter
fn first_error_line(log: &str) -> Option<String> {
    log.lines()
        .map(str::trim)
        .find(|line| !line.starts_with('[') && line.to_ascii_lowercase().contains("error"))
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
}

// a g2g run that did not decode, as the class and note every mode files it under
fn g2g_failure(run: &RunResult, g2g_stderr: &str) -> Option<(TriageClass, String)> {
    let failure_note = || first_error_line(g2g_stderr).unwrap_or_else(|| status_note(run));
    match outcome(run) {
        Outcome::Decoded => None,
        Outcome::Crashed | Outcome::Hung => Some((TriageClass::G2gCrashOrHang, status_note(run))),
        Outcome::NoOutput if calliope_adapter_g2g::reports_unsupported_input(g2g_stderr) => {
            Some((TriageClass::UnsupportedCodec, failure_note()))
        }
        Outcome::NoOutput => Some((TriageClass::G2gNoOutput, failure_note())),
        Outcome::Unhashable => Some((TriageClass::G2gDiverges, status_note(run))),
    }
}

// classify one scenario against the g2g run's stderr log
pub fn classify(report: &ScenarioReport, g2g_stderr: &str) -> (TriageClass, String) {
    let find = |engine: &str| {
        report
            .runs
            .iter()
            .map(|r| &r.run)
            .find(|r| r.engine == engine)
    };
    let (Some(reference), g2g, gst) = (
        find(REFERENCE_ENGINE),
        find(G2G_ENGINE),
        find(GSTREAMER_ENGINE),
    ) else {
        return (TriageClass::Agree, "reference did not run".into());
    };
    let g2g_outcome = g2g.map(outcome);
    let gst_outcome = gst.map(outcome);
    if let (Some(run), Some(Outcome::Crashed | Outcome::Hung)) = (g2g, g2g_outcome) {
        return (TriageClass::G2gCrashOrHang, status_note(run));
    }

    if outcome(reference) != Outcome::Decoded {
        let reference_note = format!("{REFERENCE_ENGINE} {}", status_note(reference));
        return match (g2g_outcome, gst_outcome) {
            (Some(Outcome::Decoded), Some(Outcome::Decoded)) => (
                TriageClass::ReferenceQuirk,
                format!("{reference_note}, gstreamer and g2g decoded"),
            ),
            (Some(Outcome::Decoded), _) => (
                TriageClass::G2gDiverges,
                format!("lenient: decoded input {reference_note}"),
            ),
            (_, Some(Outcome::Crashed | Outcome::Hung)) => (
                TriageClass::GstreamerDiverges,
                status_note(gst.expect("outcome implies a run")),
            ),
            _ => (
                TriageClass::Agree,
                format!("all rejected, {reference_note}"),
            ),
        };
    }

    if let Some(run) = g2g {
        if let Some(failure) = g2g_failure(run, g2g_stderr) {
            return failure;
        }
        if let Some(note) = divergence(report, run) {
            let reference_outlier = report.majority.as_ref().is_some_and(|vote| {
                vote.conclusive && vote.is_outlier(REFERENCE_ENGINE) && !vote.is_outlier(G2G_ENGINE)
            });
            if reference_outlier {
                return (
                    TriageClass::ReferenceQuirk,
                    format!("{REFERENCE_ENGINE} is the outlier, {note}"),
                );
            }
            return (TriageClass::G2gDiverges, note);
        }
    }

    if let (Some(run), Some(gst_outcome)) = (gst, gst_outcome) {
        if gst_outcome != Outcome::Decoded {
            return (TriageClass::GstreamerDiverges, status_note(run));
        }
        if let Some(note) = divergence(report, run) {
            return (TriageClass::GstreamerDiverges, note);
        }
    }
    (TriageClass::Agree, String::new())
}

#[derive(Debug, Serialize)]
pub struct SweepEntry {
    pub vector: String,
    pub kind: StreamKind,
    pub codec: String,
    pub class: TriageClass,
    pub note: String,
    // the g2g-launch command that reproduces this scenario
    pub reproducer: Option<String>,
    pub report: ScenarioReport,
}

impl SweepMode {
    // what the mode compares g2g against, for the triage header
    fn reference(self) -> String {
        match self {
            Self::Decode => REFERENCE_ENGINE.into(),
            Self::Timestamps => "ffprobe -show_frames, gstreamer breaks a tie".into(),
            Self::ChunkSize => format!("g2g at blocksize {}", CHUNK_SIZES[0]),
        }
    }
}

// the scenarios one case runs as under `mode`
fn mode_scenarios(mode: SweepMode, scenario: &Scenario) -> Vec<Scenario> {
    let g2g_alone = || Scenario {
        engines: vec![G2G_ENGINE.into()],
        reference: G2G_ENGINE.into(),
        ..scenario.clone()
    };
    match mode {
        SweepMode::Decode => vec![scenario.clone()],
        SweepMode::Timestamps => vec![Scenario {
            engines: vec![GSTREAMER_ENGINE.into(), G2G_ENGINE.into()],
            determinism: None,
            timestamps: true,
            ..g2g_alone()
        }],
        // a whole-output hash from one run per size, so any stream can be compared, packed or not
        SweepMode::ChunkSize => CHUNK_SIZES
            .iter()
            .map(|&size| Scenario {
                id: format!("{}/blocksize-{size}", scenario.id),
                blocksize: Some(size),
                determinism: Some(Determinism {
                    runs: 1,
                    threads: false,
                }),
                ..g2g_alone()
            })
            .collect(),
    }
}

fn run_stderr(run: &RunResult) -> String {
    std::fs::read_to_string(run.log_dir.join("stderr.log")).unwrap_or_default()
}

fn seconds_text(nanoseconds: u64) -> String {
    const NANOSECONDS_PER_SECOND: u64 = 1_000_000_000;
    format!(
        "{}.{:09} s",
        nanoseconds / NANOSECONDS_PER_SECOND,
        nanoseconds % NANOSECONDS_PER_SECOND
    )
}

fn reference_seconds_text(time_base: TimeBase, ticks: i64) -> String {
    format!("{:.9} s", time_base.seconds(i128::from(ticks)))
}

// one engine's checksumsink decode in a timestamps run
#[derive(Debug)]
pub struct TimestampRun<'a> {
    pub run: &'a RunResult,
    pub stderr: String,
    pub stdout: String,
}

impl TimestampRun<'_> {
    // the pts of every frame, None when the run did not finish with a parsed log
    fn pts(&self) -> Option<Vec<Option<u64>>> {
        if outcome(self.run) != Outcome::Decoded {
            return None;
        }
        let lines = parse_checksum_lines(&self.stdout)
            .expect("the runner parsed a decoded run's log into frame hashes");
        Some(lines.into_iter().map(|line| line.pts_ns).collect())
    }
}

// PCM packets are whatever the demuxer reads at once, other codecs code a fixed frame
pub fn frame_pairing(kind: StreamKind, codec: &str) -> FramePairing {
    if kind == StreamKind::Audio && codec.starts_with(PCM_CODEC_PREFIX) {
        FramePairing::FirstFrame
    } else {
        FramePairing::EveryFrame
    }
}

// classify g2g's per-frame pts against ffprobe's, gstreamer stamping every frame as g2g does makes it a reference quirk
pub fn classify_timestamps(
    g2g: &TimestampRun,
    gstreamer: Option<&TimestampRun>,
    reference: calliope_core::Result<ReferenceTimestamps>,
    pairing: FramePairing,
) -> (TriageClass, String) {
    if let Some(failure) = g2g_failure(g2g.run, &g2g.stderr) {
        return failure;
    }
    let reference = match reference {
        Ok(reference) if !reference.frames.is_empty() => reference,
        Ok(_) => {
            return (
                TriageClass::ReferenceQuirk,
                "ffprobe decodes no frames".into(),
            );
        }
        Err(e) => return (TriageClass::ReferenceQuirk, e.to_string()),
    };
    let g2g_pts = g2g.pts().expect("g2g decoded");
    let frames = reference.frames.len();
    let comparison = timestamps::compare(&reference, &g2g_pts, pairing);
    let offset_seconds = reference.time_base.seconds(comparison.offset_ticks);
    let has_offset = comparison.offset_ticks != 0;
    let offset = if has_offset {
        format!(", after taking off g2g's offset of {offset_seconds:+.9} s")
    } else {
        String::new()
    };
    let note = match comparison.verdict {
        TimestampVerdict::Match if !has_offset => return (TriageClass::Agree, String::new()),
        TimestampVerdict::Match => format!("every frame is offset by {offset_seconds:+.9} s"),
        TimestampVerdict::NoReferenceTimestamps => {
            return (
                TriageClass::Agree,
                "ffprobe has no timestamps, g2g's increase".into(),
            );
        }
        TimestampVerdict::PtsDiffers {
            frame,
            g2g_ns,
            reference_ticks,
            reference_guessed,
        } => {
            let g2g = g2g_ns.map_or("no pts".into(), seconds_text);
            let ffprobe = reference_seconds_text(reference.time_base, reference_ticks);
            let guessed = if reference_guessed {
                " where ffmpeg guessed the pts"
            } else {
                ""
            };
            format!(
                "pts differs at frame {frame} of {frames}{guessed}{offset}: g2g {g2g}, ffprobe {ffprobe}"
            )
        }
        TimestampVerdict::FrameCountDiffers {
            frames,
            reference_frames,
        } => format!(
            "{frames} frames vs ffprobe {reference_frames}, the common frames agree{offset}"
        ),
        TimestampVerdict::NotIncreasing { frame } => {
            format!(
                "g2g's pts does not increase at frame {frame} of {}",
                g2g_pts.len()
            )
        }
    };
    let gstreamer_agrees = gstreamer
        .and_then(TimestampRun::pts)
        .is_some_and(|gstreamer_pts| {
            timestamps::same_ticks(reference.time_base, &g2g_pts, &gstreamer_pts)
        });
    if gstreamer_agrees {
        return (
            TriageClass::ReferenceQuirk,
            format!("gstreamer agrees with g2g, {note}"),
        );
    }
    (TriageClass::G2gDiverges, note)
}

// what one block size's run produced, for a chunk-size note
fn chunk_size_result(run: &RunResult, g2g_stderr: &str) -> String {
    match (outcome(run), run.output_len) {
        (Outcome::Decoded, Some(bytes)) => format!("{bytes} bytes"),
        _ => first_error_line(g2g_stderr).unwrap_or_else(|| status_note(run)),
    }
}

#[derive(Debug)]
pub struct ChunkSizeRun<'a> {
    pub blocksize: u32,
    pub run: &'a RunResult,
    pub stderr: String,
}

#[derive(Debug)]
pub struct ChunkSizeTriage {
    pub class: TriageClass,
    pub note: String,
    // index of the run the note is about, its pipeline is the reproducer
    pub run: usize,
}

// classify one stream's runs at each of CHUNK_SIZES, baseline first
pub fn classify_chunk_size(runs: &[ChunkSizeRun]) -> ChunkSizeTriage {
    if let Some(index) = runs
        .iter()
        .position(|r| matches!(outcome(r.run), Outcome::Crashed | Outcome::Hung))
    {
        return ChunkSizeTriage {
            class: TriageClass::G2gCrashOrHang,
            note: format!(
                "{} at blocksize {}",
                status_note(runs[index].run),
                runs[index].blocksize
            ),
            run: index,
        };
    }
    let baseline = &runs[0];
    let differs = |run: &RunResult| {
        outcome(run) != outcome(baseline.run)
            || (outcome(run) == Outcome::Decoded && run.output_md5 != baseline.run.output_md5)
    };
    if let Some(index) = runs.iter().position(|r| differs(r.run)) {
        let other = &runs[index];
        let result = chunk_size_result(other.run, &other.stderr);
        let baseline_result = chunk_size_result(baseline.run, &baseline.stderr);
        let note = if result == baseline_result {
            format!(
                "blocksize {} and {}: {result} each, different bytes",
                other.blocksize, baseline.blocksize
            )
        } else {
            format!(
                "blocksize {}: {result}, blocksize {}: {baseline_result}",
                other.blocksize, baseline.blocksize
            )
        };
        return ChunkSizeTriage {
            class: TriageClass::G2gDiverges,
            note,
            run: index,
        };
    }
    let (class, note) =
        g2g_failure(baseline.run, &baseline.stderr).unwrap_or((TriageClass::Agree, String::new()));
    ChunkSizeTriage {
        class,
        note,
        run: 0,
    }
}

pub async fn run(
    manifest_path: &Path,
    engines: &[String],
    mode: SweepMode,
    limit: usize,
    jobs: usize,
    workdir: &Path,
) -> Result<Vec<SweepEntry>> {
    let manifest = Manifest::load(manifest_path)?;
    let mut vectors: Vec<Vector> = manifest
        .vector
        .into_iter()
        .filter(|v| v.media.is_some())
        .collect();
    if limit > 0 {
        vectors.truncate(limit);
    }
    let cases = sweep_cases(&vectors, engines);
    println!(
        "sweep: {} vectors, {} streams, mode {mode:?}",
        vectors.len(),
        cases.len(),
    );
    let cache = corpus::cache_dir();
    let g2g = crate::engine_by_id(G2G_ENGINE)?;
    let mut entries = Vec::new();
    for batch in cases.chunks(SWEEP_BATCH) {
        let planned: Vec<Vec<Scenario>> = batch
            .iter()
            .map(|case| mode_scenarios(mode, &case.scenario))
            .collect();
        let scenarios: Vec<Scenario> = planned.iter().flatten().cloned().collect();
        let mut reports = crate::run_matrix(&scenarios, manifest_path, &[], jobs, workdir)
            .await?
            .scenarios;
        let mut finished = Vec::new();
        for (case, case_scenarios) in batch.iter().zip(planned) {
            let case_reports: Vec<(Scenario, ScenarioReport)> = case_scenarios
                .into_iter()
                .filter_map(|scenario| {
                    let index = reports.iter().position(|r| r.scenario == scenario.id)?;
                    Some((scenario, reports.remove(index)))
                })
                .collect();
            // run_matrix skips a vector it could not fetch
            if !case_reports.is_empty() {
                finished.push((case, case_reports));
            }
        }
        let references = match mode {
            SweepMode::Timestamps => {
                let probes = finished.iter().map(|(case, _)| {
                    let input = cache.join(&case.vector);
                    async move {
                        let timeout = std::time::Duration::from_secs(SWEEP_TIMEOUT_SECS);
                        Some(probe_frame_timestamps(&input, case.kind, timeout).await)
                    }
                });
                futures::future::join_all(probes).await
            }
            SweepMode::Decode | SweepMode::ChunkSize => finished.iter().map(|_| None).collect(),
        };
        for ((case, mut case_reports), reference) in finished.into_iter().zip(references) {
            let (class, note, reproduced, report) = match mode {
                SweepMode::Decode => {
                    let (scenario, report) = case_reports.remove(0);
                    let g2g_run = report.runs.iter().find(|r| r.run.engine == G2G_ENGINE);
                    let stderr = g2g_run.map(|r| run_stderr(&r.run)).unwrap_or_default();
                    let (class, note) = classify(&report, &stderr);
                    let reproduced = g2g_run.is_some().then_some(scenario);
                    (class, note, reproduced, report)
                }
                SweepMode::Timestamps => {
                    let (scenario, report) = case_reports.remove(0);
                    let timestamp_run = |engine: &str| -> Result<Option<TimestampRun>> {
                        let Some(entry) = report.runs.iter().find(|r| r.run.engine == engine)
                        else {
                            return Ok(None);
                        };
                        Ok(Some(TimestampRun {
                            run: &entry.run,
                            stderr: run_stderr(&entry.run),
                            stdout: std::fs::read_to_string(entry.run.log_dir.join(STDOUT_LOG))?,
                        }))
                    };
                    let g2g = timestamp_run(G2G_ENGINE)?.expect("timestamps runs g2g");
                    let gstreamer = timestamp_run(GSTREAMER_ENGINE)?;
                    let reference = reference.expect("timestamps mode probes every case");
                    let pairing = frame_pairing(case.kind, &case.codec);
                    let (class, note) =
                        classify_timestamps(&g2g, gstreamer.as_ref(), reference, pairing);
                    (class, note, Some(scenario), report)
                }
                SweepMode::ChunkSize => {
                    let runs: Vec<ChunkSizeRun> = case_reports
                        .iter()
                        .map(|(scenario, report)| ChunkSizeRun {
                            blocksize: scenario.blocksize.expect("chunk-size scenarios set one"),
                            run: &report.runs[0].run,
                            stderr: run_stderr(&report.runs[0].run),
                        })
                        .collect();
                    let ChunkSizeTriage { class, note, run } = classify_chunk_size(&runs);
                    let reproduced = case_reports[run].0.clone();
                    let mut merged = case_reports.into_iter().map(|(_, report)| report);
                    let mut report = merged.next().expect("one report per chunk size");
                    report.runs.extend(merged.flat_map(|other| other.runs));
                    report.scenario = case.scenario.id.clone();
                    (class, note, Some(reproduced), report)
                }
            };
            let reproducer = reproduced.and_then(|scenario| {
                reproducer(g2g.as_ref(), &scenario, &cache.join(&case.vector))
            });
            for run in &report.runs {
                delete_decoded_output(&run.run.log_dir)?;
            }
            println!("  {:<18} {}", class.title(), report.scenario);
            entries.push(SweepEntry {
                vector: case.vector.clone(),
                kind: case.kind,
                codec: case.codec.clone(),
                class,
                note,
                reproducer,
                report,
            });
        }
    }
    Ok(entries)
}

fn reproducer(
    engine: &dyn calliope_core::Engine,
    scenario: &Scenario,
    input: &Path,
) -> Option<String> {
    let invocation = engine
        .plan(scenario, input, &PathBuf::from(REPRODUCER_DIR))
        .ok()?;
    Some(format!(
        "{} {}",
        invocation.program,
        invocation.args.join(" ")
    ))
}

// keep the logs, drop the raw decodes, which add up to more than the corpus
fn delete_decoded_output(run_dir: &Path) -> std::io::Result<()> {
    let Ok(entries) = std::fs::read_dir(run_dir) else {
        return Ok(());
    };
    for entry in entries {
        let path = entry?.path();
        if path.is_file() && path.extension().is_none_or(|e| e != RUN_LOG_EXTENSION) {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

// a note with its numbers masked, so failures that differ only in a frame index or rate group together
fn note_signature(note: &str) -> String {
    let mut signature = String::with_capacity(note.len());
    for c in note.chars() {
        if !c.is_ascii_digit() {
            signature.push(c);
        } else if !signature.ends_with('N') {
            signature.push('N');
        }
    }
    signature
}

fn table_cell(text: &str) -> String {
    text.replace('|', "/")
}

// markdown triage table: class counts, then one section per non-agree class
pub fn triage_markdown(title: &str, mode: SweepMode, entries: &[SweepEntry]) -> String {
    let mut by_class: BTreeMap<TriageClass, Vec<&SweepEntry>> = BTreeMap::new();
    for entry in entries {
        by_class.entry(entry.class).or_default().push(entry);
    }
    let vectors: std::collections::BTreeSet<&str> =
        entries.iter().map(|e| e.vector.as_str()).collect();
    let mut out = format!(
        "# {title} sweep triage\n\n{} files, {} scenarios (one per video / audio stream), reference {}.\n\n| class | scenarios |\n| :--- | ---: |\n",
        vectors.len(),
        entries.len(),
        mode.reference()
    );
    for (class, rows) in &by_class {
        let _ = writeln!(out, "| {} | {} |", class.title(), rows.len());
    }
    for (class, rows) in &by_class {
        let heading = class.title();
        match class {
            TriageClass::Agree => {}
            // every g2g divergence gets its own row, diagnosis, and reproducer
            TriageClass::G2gCrashOrHang | TriageClass::G2gDiverges => {
                let _ = write!(
                    out,
                    "\n## {heading}\n\n| scenario | codec | result | diagnosis | reproducer |\n| :--- | :--- | :--- | :--- | :--- |\n"
                );
                for row in rows {
                    let _ = writeln!(
                        out,
                        "| `{}` | {} | {} | | `{}` |",
                        row.report.scenario,
                        row.codec,
                        table_cell(&row.note),
                        row.reproducer
                            .as_deref()
                            .map(table_cell)
                            .unwrap_or_default()
                    );
                }
            }
            TriageClass::UnsupportedCodec => {
                let mut codecs: BTreeMap<&str, usize> = BTreeMap::new();
                for row in rows {
                    *codecs.entry(row.codec.as_str()).or_default() += 1;
                }
                let _ = write!(
                    out,
                    "\n## {heading}\n\n| codec | scenarios |\n| :--- | ---: |\n"
                );
                for (codec, count) in codecs {
                    let _ = writeln!(out, "| {codec} | {count} |");
                }
            }
            _ => {
                let mut groups: BTreeMap<String, Vec<&SweepEntry>> = BTreeMap::new();
                for row in rows {
                    groups
                        .entry(note_signature(&row.note))
                        .or_default()
                        .push(row);
                }
                let mut groups: Vec<_> = groups.into_iter().collect();
                groups.sort_by_key(|(_, rows)| std::cmp::Reverse(rows.len()));
                let _ = write!(
                    out,
                    "\n## {heading}\n\n| result | scenarios | codecs | example | diagnosis |\n| :--- | ---: | :--- | :--- | :--- |\n"
                );
                for (signature, rows) in groups {
                    let codecs: std::collections::BTreeSet<&str> =
                        rows.iter().map(|r| r.codec.as_str()).collect();
                    let codecs: Vec<&str> = codecs.into_iter().collect();
                    let _ = writeln!(
                        out,
                        "| {} | {} | {} | `{}` | |",
                        table_cell(&signature),
                        rows.len(),
                        codecs.join(", "),
                        rows[0].report.scenario
                    );
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use calliope_core::compare::{Comparison, majority_vote};
    use calliope_core::corpus::{AudioStream, Media, VideoStream};
    use calliope_core::report::EngineReport;

    fn engines() -> Vec<String> {
        [REFERENCE_ENGINE, GSTREAMER_ENGINE, G2G_ENGINE]
            .map(String::from)
            .to_vec()
    }

    fn vector(id: &str, video: Option<(&str, &str)>, audio: Option<&str>) -> Vector {
        Vector {
            id: id.into(),
            url: "u".into(),
            sha256: Some("00".into()),
            md5: None,
            archive_member: None,
            decoded_md5: None,
            output_format: None,
            license: "l".into(),
            notes: String::new(),
            media: Some(Media {
                container: "c".into(),
                video: video.map(|(codec, pix_fmt)| VideoStream {
                    codec: codec.into(),
                    width: 64,
                    height: 48,
                    pix_fmt: Some(pix_fmt.into()),
                }),
                audio: audio.map(|codec| AudioStream {
                    codec: codec.into(),
                    sample_rate: 44100,
                    channels: 2,
                }),
            }),
        }
    }

    #[test]
    fn planar_video_is_a_per_frame_differential() {
        for pix_fmt in ["yuv420p", "yuv422p10le", "yuv444p12le"] {
            let cases = sweep_cases(&[vector("v", Some(("hevc", pix_fmt)), None)], &engines());
            let scenario = &cases[0].scenario;
            assert!(scenario.judges_frames(), "{pix_fmt}");
            let video = scenario.video.unwrap();
            assert_eq!((video.width, video.height), (64, 48));
            assert_eq!(video.format.ffmpeg_pix_fmt(), pix_fmt);
            assert_eq!(scenario.engines, engines());
        }
    }

    #[test]
    fn packed_video_self_compares() {
        let cases = sweep_cases(&[vector("v", Some(("png", "rgb24")), None)], &engines());
        let scenario = &cases[0].scenario;
        assert!(scenario.is_determinism() && scenario.video.is_none());
        assert_eq!(cases[0].codec, "png rgb24");
    }

    #[test]
    fn audio_follows_the_per_codec_bit_exact_policy() {
        let case = |codec| {
            sweep_cases(&[vector("a", None, Some(codec))], &engines())
                .remove(0)
                .scenario
        };
        let flac = case("flac");
        assert!(flac.judges_frames() && flac.engines == engines());
        let audio = flac.audio.unwrap();
        assert_eq!((audio.rate, audio.channels), (44100, 2));
        // ac3 is bit-exact only through a shared libavcodec, gstreamer drops out
        let ac3 = case("ac3");
        assert!(ac3.judges_frames());
        assert!(!ac3.engines.iter().any(|e| e == GSTREAMER_ENGINE));
        for codec in ["aac", "vorbis", "mp3"] {
            assert!(case(codec).is_determinism(), "{codec}");
        }
    }

    #[test]
    fn one_case_per_stream_and_unprobed_or_spaced_vectors_skipped() {
        let mut unprobed = vector("bare", None, None);
        unprobed.media = None;
        let vectors = [
            vector("av", Some(("h264", "yuv420p")), Some("aac")),
            unprobed,
            vector("has space.png", Some(("png", "rgb24")), None),
        ];
        let ids: Vec<String> = sweep_cases(&vectors, &engines())
            .into_iter()
            .map(|c| c.scenario.id)
            .collect();
        assert_eq!(ids, ["av/video", "av/audio"]);
    }

    fn run(engine: &str, status: RunStatus, hashes: Option<&[&str]>) -> RunResult {
        RunResult {
            engine: engine.into(),
            status,
            duration_ms: 0,
            peak_rss_kb: None,
            frame_hashes: hashes.map(|h| h.iter().map(|s| s.to_string()).collect()),
            log_dir: PathBuf::new(),
            iterations_completed: None,
            output_md5: None,
            output_len: None,
            determinism_matched: None,
            psnr: None,
            decoded: Some(hashes.is_some()),
            output: None,
            validations: Vec::new(),
        }
    }

    // a differential report built the way run_matrix builds it
    fn differential(runs: Vec<RunResult>) -> ScenarioReport {
        let voters: Vec<(String, Vec<String>)> = runs
            .iter()
            .filter_map(|r| r.frame_hashes.clone().map(|h| (r.engine.clone(), h)))
            .collect();
        let reference = runs[0].frame_hashes.clone();
        ScenarioReport {
            scenario: "s".into(),
            reference: REFERENCE_ENGINE.into(),
            robustness: false,
            soak: false,
            determinism: false,
            audio: false,
            outcome_diff: false,
            produce: false,
            golden_expected: None,
            majority: (voters.len() >= 3).then(|| majority_vote(&voters)),
            roundtrip_psnr_min: None,
            resolution_change_expected_bytes: None,
            runs: runs
                .into_iter()
                .map(|run| {
                    let comparison: Option<Comparison> = match (&reference, &run.frame_hashes) {
                        (Some(r), Some(c)) if run.engine != REFERENCE_ENGINE => {
                            Some(calliope_core::compare::compare(r, c))
                        }
                        _ => None,
                    };
                    EngineReport { run, comparison }
                })
                .collect(),
        }
    }

    const GOOD: &[&str] = &["a", "b"];
    const BAD: &[&str] = &["a", "x"];

    #[test]
    fn classifies_frame_divergences_by_majority() {
        let class = |ffmpeg, gst, g2g| {
            classify(
                &differential(vec![
                    run(REFERENCE_ENGINE, RunStatus::Ok, Some(ffmpeg)),
                    run(GSTREAMER_ENGINE, RunStatus::Ok, Some(gst)),
                    run(G2G_ENGINE, RunStatus::Ok, Some(g2g)),
                ]),
                "",
            )
            .0
        };
        assert_eq!(class(GOOD, GOOD, GOOD), TriageClass::Agree);
        assert_eq!(class(GOOD, GOOD, BAD), TriageClass::G2gDiverges);
        assert_eq!(class(GOOD, BAD, GOOD), TriageClass::GstreamerDiverges);
        assert_eq!(class(BAD, GOOD, GOOD), TriageClass::ReferenceQuirk);
    }

    #[test]
    fn classifies_g2g_failures() {
        let with_g2g = |g2g: RunResult, stderr: &str| {
            classify(
                &differential(vec![
                    run(REFERENCE_ENGINE, RunStatus::Ok, Some(GOOD)),
                    run(GSTREAMER_ENGINE, RunStatus::Ok, Some(GOOD)),
                    g2g,
                ]),
                stderr,
            )
        };
        const NO_DECODER: &str = "[h264 @ 0x1] error while decoding MB 0 0\nparse error: decodebin: no decoder chain from audio/x-raw,format=U8 to raw";
        const CAPS_FAILURE: &str = "ERROR caps   no caps overlap between AviDemux and CapsFilter\npipeline error: CapsMismatch";
        let rejected = || run(G2G_ENGINE, RunStatus::ExitFailure { code: 1 }, None);
        let (class, note) = with_g2g(rejected(), NO_DECODER);
        assert_eq!(class, TriageClass::UnsupportedCodec);
        // the note is g2g's own error, not libav's decoder chatter before it
        assert!(note.starts_with("parse error: decodebin"), "{note}");
        const NO_STREAM_DECODER: &str =
            "parse error: decodebin: no decoder for sowt in /media/surge-2-16-L-sowt.mov";
        assert_eq!(
            with_g2g(rejected(), NO_STREAM_DECODER).0,
            TriageClass::UnsupportedCodec
        );
        let (class, note) = with_g2g(rejected(), CAPS_FAILURE);
        assert_eq!(class, TriageClass::G2gNoOutput);
        assert_eq!(
            note,
            "ERROR caps no caps overlap between AviDemux and CapsFilter"
        );
        let crashed = run(G2G_ENGINE, RunStatus::Signaled { signal: 11 }, None);
        assert_eq!(with_g2g(crashed, "").0, TriageClass::G2gCrashOrHang);
        // output that is not a whole number of frames is a divergence, not a rejection
        let mut partial = run(
            G2G_ENGINE,
            RunStatus::Error {
                message: "trailing partial frame".into(),
            },
            None,
        );
        partial.decoded = Some(true);
        assert_eq!(with_g2g(partial, "").0, TriageClass::G2gDiverges);
    }

    #[test]
    fn reference_rejection_uses_the_outcome_oracle() {
        let class = |gst: Option<&[&str]>, g2g: Option<&[&str]>| {
            let status = |h: Option<&[&str]>| match h {
                Some(_) => RunStatus::Ok,
                None => RunStatus::ExitFailure { code: 1 },
            };
            classify(
                &differential(vec![
                    run(REFERENCE_ENGINE, RunStatus::ExitFailure { code: 1 }, None),
                    run(GSTREAMER_ENGINE, status(gst), gst),
                    run(G2G_ENGINE, status(g2g), g2g),
                ]),
                "",
            )
            .0
        };
        assert_eq!(class(None, None), TriageClass::Agree);
        assert_eq!(class(None, Some(GOOD)), TriageClass::G2gDiverges);
        assert_eq!(class(Some(GOOD), Some(GOOD)), TriageClass::ReferenceQuirk);
    }

    #[test]
    fn determinism_divergence_is_a_run_to_run_difference() {
        let mut report = differential(vec![
            run(REFERENCE_ENGINE, RunStatus::Ok, Some(GOOD)),
            run(G2G_ENGINE, RunStatus::Ok, Some(GOOD)),
        ]);
        report.determinism = true;
        for entry in &mut report.runs {
            entry.run.determinism_matched = Some(entry.run.engine != G2G_ENGINE);
        }
        assert_eq!(classify(&report, "").0, TriageClass::G2gDiverges);
    }

    #[test]
    fn triage_groups_failures_that_differ_only_in_numbers() {
        let entry = |id: &str, class, note: &str| SweepEntry {
            vector: id.into(),
            kind: StreamKind::Audio,
            codec: "pcm_u8".into(),
            class,
            note: note.into(),
            reproducer: None,
            report: differential(vec![run(REFERENCE_ENGINE, RunStatus::Ok, Some(GOOD))]),
        };
        let entries = [
            entry("a", TriageClass::G2gNoOutput, "no caps at 22050 Hz"),
            entry("b", TriageClass::G2gNoOutput, "no caps at 44100 Hz"),
            entry("c", TriageClass::Agree, ""),
        ];
        let markdown = triage_markdown("t", SweepMode::Decode, &entries);
        assert!(markdown.contains("| g2g no output | 2 |"), "{markdown}");
        assert!(
            markdown.contains("| no caps at N Hz | 2 | pcm_u8 |"),
            "{markdown}"
        );
        // agreeing scenarios are counted, never listed
        assert!(!markdown.contains("## agree"), "{markdown}");
    }

    #[test]
    fn timestamp_and_chunk_size_modes_decode_into_their_sinks() {
        use calliope_core::Engine;
        use calliope_core::engine::OutputSpec;
        let case = sweep_cases(
            &[vector("v", Some(("png", "rgb24")), Some("aac"))],
            &engines(),
        );
        let input = Path::new("/media/v");
        let workdir = Path::new("/run");
        let pipeline = |scenario: &Scenario| {
            calliope_adapter_g2g::G2g
                .plan(scenario, input, workdir)
                .unwrap()
                .args
                .join(" ")
        };
        for case in &case {
            let timestamps = mode_scenarios(SweepMode::Timestamps, &case.scenario).remove(0);
            assert_eq!(timestamps.engines, [GSTREAMER_ENGINE, G2G_ENGINE]);
            assert!(!timestamps.is_determinism());
            let plan = calliope_adapter_g2g::G2g
                .plan(&timestamps, input, workdir)
                .unwrap();
            assert!(plan.args.join(" ").ends_with("! checksumsink hash=md5"));
            assert!(
                matches!(plan.output, OutputSpec::ChecksumLines(path) if path == workdir.join(STDOUT_LOG))
            );

            let chunked = mode_scenarios(SweepMode::ChunkSize, &case.scenario);
            let sizes: Vec<Option<u32>> = chunked.iter().map(|s| s.blocksize).collect();
            assert_eq!(sizes, CHUNK_SIZES.map(Some));
            for scenario in &chunked {
                assert!(scenario.is_determinism() && scenario.engines == [G2G_ENGINE]);
                let size = scenario.blocksize.unwrap();
                assert_eq!(
                    scenario.id,
                    format!("{}/blocksize-{size}", case.scenario.id)
                );
                assert!(
                    pipeline(scenario).contains(&format!("location=/media/v blocksize={size} !")),
                    "{}",
                    pipeline(scenario)
                );
            }
        }
        // the decode mode keeps the default block size
        let decode = mode_scenarios(SweepMode::Decode, &case[0].scenario).remove(0);
        assert!(!pipeline(&decode).contains("blocksize"));
    }

    const TICKS_PER_SECOND: u64 = 90_000;
    const NANOSECONDS_PER_TICK: u64 = 1_000_000_000 / TICKS_PER_SECOND;

    // ffprobe's frames at 1/90000, one per tick in `ticks`
    fn reference_timestamps(ticks: &[i64]) -> ReferenceTimestamps {
        ReferenceTimestamps {
            time_base: TimeBase::parse(&format!("1/{TICKS_PER_SECOND}")).unwrap(),
            frames: ticks
                .iter()
                .map(|&t| timestamps::ReferenceFrame {
                    pts: Some(t),
                    best_effort_timestamp: Some(t),
                })
                .collect(),
        }
    }

    // g2g-launch's checksumsink lines for frames at `ticks`
    fn checksum_stdout(ticks: &[u64]) -> String {
        ticks
            .iter()
            .map(|t| {
                let ns = t * NANOSECONDS_PER_TICK;
                format!(
                    "0:00:{:02}.{:09} 00\n",
                    ns / 1_000_000_000,
                    ns % 1_000_000_000
                )
            })
            .collect()
    }

    fn decoded_at(ticks: &[u64]) -> (RunResult, String) {
        (
            run(G2G_ENGINE, RunStatus::Ok, Some(GOOD)),
            checksum_stdout(ticks),
        )
    }

    fn timestamp_run((run, stdout): &(RunResult, String)) -> TimestampRun<'_> {
        TimestampRun {
            run,
            stderr: String::new(),
            stdout: stdout.clone(),
        }
    }

    #[test]
    fn classifies_timestamps_against_ffprobe() {
        let reference = || Ok(reference_timestamps(&[0, 3000]));
        let class = |g2g_ticks: &[u64], gstreamer_ticks: &[u64]| {
            let (g2g, gstreamer) = (decoded_at(g2g_ticks), decoded_at(gstreamer_ticks));
            classify_timestamps(
                &timestamp_run(&g2g),
                Some(&timestamp_run(&gstreamer)),
                reference(),
                FramePairing::EveryFrame,
            )
        };
        assert_eq!(class(&[0, 3000], &[0, 3000]).0, TriageClass::Agree);
        let (late, note) = class(&[0, 3600], &[0, 3000]);
        assert_eq!(late, TriageClass::G2gDiverges);
        assert!(note.starts_with("pts differs at frame 1 of 2"), "{note}");
        let (extra, note) = class(&[0, 3000, 6000], &[0, 3000]);
        assert_eq!(extra, TriageClass::G2gDiverges);
        assert_eq!(note, "3 frames vs ffprobe 2, the common frames agree");
        let (shifted, note) = class(&[900, 3900], &[0, 3000]);
        assert_eq!(shifted, TriageClass::G2gDiverges);
        assert_eq!(note, "every frame is offset by +0.010000000 s");
        let (late_after_shift, note) = class(&[900, 4500], &[0, 3000]);
        assert_eq!(late_after_shift, TriageClass::G2gDiverges);
        assert!(
            note.contains("after taking off g2g's offset of +0.010000000 s"),
            "{note}"
        );
        // ffprobe is the outlier when gstreamer stamps the frames as g2g does
        let (quirk, note) = class(&[0, 3600], &[0, 3600]);
        assert_eq!(quirk, TriageClass::ReferenceQuirk);
        assert!(note.starts_with("gstreamer agrees with g2g"), "{note}");

        let mut guessed = reference_timestamps(&[0, 3000]);
        guessed.frames[1].pts = None;
        let g2g = decoded_at(&[0, 3600]);
        let (_, note) = classify_timestamps(
            &timestamp_run(&g2g),
            None,
            Ok(guessed),
            FramePairing::EveryFrame,
        );
        assert!(note.contains("where ffmpeg guessed the pts"), "{note}");

        // a run g2g could not decode is filed the way the decode sweep files it
        let rejected = run(G2G_ENGINE, RunStatus::ExitFailure { code: 1 }, None);
        let rejected = TimestampRun {
            run: &rejected,
            stderr: "parse error: decodebin: no decoder for sowt in /media/a.mov".into(),
            stdout: String::new(),
        };
        assert_eq!(
            classify_timestamps(&rejected, None, reference(), FramePairing::EveryFrame).0,
            TriageClass::UnsupportedCodec
        );
    }

    #[test]
    fn only_pcm_audio_pairs_the_first_frame_alone() {
        assert_eq!(
            frame_pairing(StreamKind::Audio, "pcm_s16le"),
            FramePairing::FirstFrame
        );
        assert_eq!(
            frame_pairing(StreamKind::Audio, "aac"),
            FramePairing::EveryFrame
        );
        assert_eq!(
            frame_pairing(StreamKind::Video, "pcm_s16le"),
            FramePairing::EveryFrame
        );
    }

    // one chunk-size run: decoded to `md5` when given, else rejected
    fn chunk_run(md5: Option<&str>) -> RunResult {
        let mut result = match md5 {
            Some(_) => run(G2G_ENGINE, RunStatus::Ok, Some(GOOD)),
            None => run(G2G_ENGINE, RunStatus::ExitFailure { code: 1 }, None),
        };
        result.output_md5 = md5.map(String::from);
        result.output_len = md5.map(|m| m.len() as u64);
        result
    }

    fn chunk_triage(results: &[RunResult]) -> ChunkSizeTriage {
        let runs: Vec<ChunkSizeRun> = CHUNK_SIZES
            .iter()
            .zip(results)
            .map(|(&blocksize, run)| ChunkSizeRun {
                blocksize,
                run,
                stderr: String::new(),
            })
            .collect();
        classify_chunk_size(&runs)
    }

    #[test]
    fn classifies_chunk_size_runs_against_the_default_block_size() {
        let same = [
            chunk_run(Some("aa")),
            chunk_run(Some("aa")),
            chunk_run(Some("aa")),
        ];
        assert_eq!(chunk_triage(&same).class, TriageClass::Agree);

        let smallest_differs = [
            chunk_run(Some("aa")),
            chunk_run(Some("aa")),
            chunk_run(Some("bb")),
        ];
        let triage = chunk_triage(&smallest_differs);
        assert_eq!(triage.class, TriageClass::G2gDiverges);
        assert_eq!(triage.run, 2);
        assert_eq!(
            triage.note,
            format!(
                "blocksize {} and {}: 2 bytes each, different bytes",
                CHUNK_SIZES[2], CHUNK_SIZES[0]
            )
        );

        let rejected_when_chunked = [
            chunk_run(Some("aa")),
            chunk_run(None),
            chunk_run(Some("aa")),
        ];
        let triage = chunk_triage(&rejected_when_chunked);
        assert_eq!((triage.class, triage.run), (TriageClass::G2gDiverges, 1));

        let mut crashed = chunk_run(None);
        crashed.status = RunStatus::Signaled { signal: 11 };
        let triage = chunk_triage(&[chunk_run(Some("aa")), chunk_run(Some("aa")), crashed]);
        assert_eq!((triage.class, triage.run), (TriageClass::G2gCrashOrHang, 2));

        // no output at every size is the decode sweep's failure, not a chunk-size one
        let never = [chunk_run(None), chunk_run(None), chunk_run(None)];
        assert_eq!(chunk_triage(&never).class, TriageClass::G2gNoOutput);
    }
}
