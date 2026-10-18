// real-world corpus sweep: one synthesized scenario per decodable stream of every imported sample

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use calliope_core::corpus::{self, Manifest, Vector};
use calliope_core::report::ScenarioReport;
use calliope_core::runner::{RunResult, RunStatus};
use calliope_core::scenario::{
    Audio, AudioCodec, Determinism, Input, PixelFormat, SampleFormat, Scenario, Video,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StreamKind {
    Video,
    Audio,
}

impl StreamKind {
    fn label(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }
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

// classify one scenario against the g2g run's stderr log
pub fn classify(report: &ScenarioReport, g2g_stderr: &str) -> (TriageClass, String) {
    let g2g_failure_note =
        |run: &RunResult| first_error_line(g2g_stderr).unwrap_or_else(|| status_note(run));
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

    if let (Some(run), Some(g2g_outcome)) = (g2g, g2g_outcome) {
        match g2g_outcome {
            Outcome::NoOutput if calliope_adapter_g2g::reports_unsupported_input(g2g_stderr) => {
                return (TriageClass::UnsupportedCodec, g2g_failure_note(run));
            }
            Outcome::NoOutput => return (TriageClass::G2gNoOutput, g2g_failure_note(run)),
            Outcome::Unhashable => return (TriageClass::G2gDiverges, status_note(run)),
            _ => {}
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

pub async fn run(
    manifest_path: &Path,
    engines: &[String],
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
        "sweep: {} vectors, {} scenarios x {} engines",
        vectors.len(),
        cases.len(),
        engines.len()
    );
    let cache = corpus::cache_dir();
    let g2g = crate::engine_by_id(G2G_ENGINE)?;
    let mut entries = Vec::new();
    for batch in cases.chunks(SWEEP_BATCH) {
        let scenarios: Vec<Scenario> = batch.iter().map(|c| c.scenario.clone()).collect();
        let report = crate::run_matrix(&scenarios, manifest_path, &[], jobs, workdir).await?;
        for scenario_report in report.scenarios {
            let case = batch
                .iter()
                .find(|c| c.scenario.id == scenario_report.scenario)
                .expect("report rows come from this batch");
            let g2g_run = scenario_report
                .runs
                .iter()
                .find(|r| r.run.engine == G2G_ENGINE);
            let g2g_stderr = g2g_run
                .and_then(|r| std::fs::read_to_string(r.run.log_dir.join("stderr.log")).ok())
                .unwrap_or_default();
            let (class, note) = classify(&scenario_report, &g2g_stderr);
            let reproducer = g2g_run
                .is_some()
                .then(|| reproducer(g2g.as_ref(), &case.scenario, &cache.join(&case.vector)))
                .flatten();
            for run in &scenario_report.runs {
                delete_decoded_output(&run.run.log_dir)?;
            }
            println!("  {:<18} {}", class.title(), scenario_report.scenario);
            entries.push(SweepEntry {
                vector: case.vector.clone(),
                kind: case.kind,
                codec: case.codec.clone(),
                class,
                note,
                reproducer,
                report: scenario_report,
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
pub fn triage_markdown(title: &str, entries: &[SweepEntry]) -> String {
    let mut by_class: BTreeMap<TriageClass, Vec<&SweepEntry>> = BTreeMap::new();
    for entry in entries {
        by_class.entry(entry.class).or_default().push(entry);
    }
    let vectors: std::collections::BTreeSet<&str> =
        entries.iter().map(|e| e.vector.as_str()).collect();
    let mut out = format!(
        "# {title} sweep triage\n\n{} files, {} scenarios (one per video / audio stream), reference {REFERENCE_ENGINE}.\n\n| class | scenarios |\n| :--- | ---: |\n",
        vectors.len(),
        entries.len()
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
        const NO_DECODER: &str = "[h264 @ 0x1] error while decoding MB 0 0\nparse error: decodebin: no decoder chain from Audio { format: PcmU8 } to raw";
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
        let markdown = triage_markdown("t", &entries);
        assert!(markdown.contains("| g2g no output | 2 |"), "{markdown}");
        assert!(
            markdown.contains("| no caps at N Hz | 2 | pcm_u8 |"),
            "{markdown}"
        );
        // agreeing scenarios are counted, never listed
        assert!(!markdown.contains("## agree"), "{markdown}");
    }
}
