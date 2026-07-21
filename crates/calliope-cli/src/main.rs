use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use futures::stream::{FuturesUnordered, StreamExt};

use calliope_core::compare::{DecodeOutcome, OutcomeVerdict, compare, outcome_verdict};
use calliope_core::corpus::{self, Manifest};
use calliope_core::engine::Engine;
use calliope_core::report::{EngineReport, Report, ScenarioReport};
use calliope_core::runner::{RunStatus, run_determinism, run_one, run_soak};
use calliope_core::scenario::{Input, Scenario, Video};

#[derive(Parser)]
#[command(name = "calliope", about = "differential media QA harness", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// probe installed engines and print versions
    Engines,
    /// download corpus vectors into the cache
    Fetch {
        #[arg(long, default_value = "corpus/vectors.toml")]
        corpus: PathBuf,
        /// vector ids; all when empty
        ids: Vec<String>,
    },
    /// import Fluster conformance suite JSONs into the corpus manifest
    CorpusImport {
        /// a Fluster test_suites directory or a single suite .json
        #[arg(long)]
        fluster: PathBuf,
        #[arg(long, default_value = "corpus/vectors.toml")]
        out: PathBuf,
        /// cap the number of imported vectors (0 = all)
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// shrink a crashing / hanging input to a minimal reproducer for one engine
    Minimize {
        /// engine that crashes or hangs on the input
        #[arg(long)]
        engine: String,
        /// the failing input file (e.g. a robustness run's input.corrupted)
        #[arg(long)]
        input: PathBuf,
        /// where to write the minimized reproducer (default: <input>.min)
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value_t = 10)]
        timeout_secs: u64,
        #[arg(long, default_value = "runs/minimize")]
        workdir: PathBuf,
    },
    /// run scenarios and compare engines against the reference
    Run {
        scenarios: Vec<PathBuf>,
        #[arg(long, default_value = "corpus/vectors.toml")]
        corpus: PathBuf,
        /// restrict to these engines (comma-separated); reference must stay
        #[arg(long, value_delimiter = ',')]
        engines: Vec<String>,
        #[arg(long, default_value_t = 4)]
        jobs: usize,
        #[arg(long, default_value = "runs")]
        workdir: PathBuf,
        /// write a JSON report here
        #[arg(long)]
        report: Option<PathBuf>,
    },
    /// golden-check every corpus vector with a decoded-md5 (a conformance run)
    Conformance {
        #[arg(long, default_value = "corpus/vectors.toml")]
        corpus: PathBuf,
        /// engines to check (comma-separated); default ffmpeg,gstreamer,g2g
        #[arg(long, value_delimiter = ',')]
        engines: Vec<String>,
        /// cap the number of vectors checked (0 = all)
        #[arg(long, default_value_t = 0)]
        limit: usize,
        #[arg(long, default_value_t = 4)]
        jobs: usize,
        #[arg(long, default_value = "runs")]
        workdir: PathBuf,
        #[arg(long)]
        report: Option<PathBuf>,
    },
}

fn all_engines() -> Vec<Arc<dyn Engine>> {
    vec![
        Arc::new(calliope_adapter_ffmpeg::Ffmpeg),
        Arc::new(calliope_adapter_gst::GStreamer),
        Arc::new(calliope_adapter_g2g::G2g),
    ]
}

fn engine_by_id(id: &str) -> Result<Arc<dyn Engine>> {
    all_engines()
        .into_iter()
        .find(|e| e.id() == id)
        .with_context(|| format!("unknown engine '{id}'"))
}

/// Run `engine` on `input` synchronously and report whether it failed the
/// robustness bar: crashed on a signal (SIGSEGV / SIGABRT) or hung past
/// `timeout`. A clean exit (success or graceful error) is not a failure. Used
/// as the minimizer's predicate.
fn engine_crashes(
    engine: &dyn Engine,
    input: &std::path::Path,
    workdir: &std::path::Path,
    timeout: std::time::Duration,
) -> bool {
    let scenario = Scenario {
        id: "minimize".into(),
        engines: vec![engine.id().into()],
        reference: engine.id().into(),
        timeout_secs: timeout.as_secs().max(1),
        input: Input {
            corpus: None,
            path: Some(input.to_path_buf()),
        },
        video: None,
        audio: None,
        fault: None,
        soak: None,
        determinism: None,
        golden: false,
        roundtrip: None,
        encode: None,
        resolution_change: false,
        outcome_diff: false,
    };
    let Ok(inv) = engine.plan(&scenario, input, workdir) else {
        return false;
    };
    let mut child = match std::process::Command::new(&inv.program)
        .args(&inv.args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return is_crash(status),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return true; // hang
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => return false,
        }
    }
}

/// Synthesize one golden scenario per corpus vector that carries a conformance
/// hash (`decoded-md5` + `output-format`), so a whole imported suite runs in one
/// `conformance` pass. The first engine is the (unused for golden) reference.
fn golden_scenarios(
    vectors: &[calliope_core::corpus::Vector],
    engines: &[String],
) -> Vec<Scenario> {
    vectors
        .iter()
        .filter(|v| v.decoded_md5.is_some() && v.output_format.is_some())
        .map(|v| Scenario {
            id: v.id.clone(),
            engines: engines.to_vec(),
            reference: engines[0].clone(),
            timeout_secs: 120,
            input: Input {
                corpus: Some(v.id.clone()),
                path: None,
            },
            video: None,
            audio: None,
            fault: None,
            soak: None,
            determinism: None,
            golden: true,
            roundtrip: None,
            encode: None,
            resolution_change: false,
            outcome_diff: false,
        })
        .collect()
}

/// A process death by signal (not a normal exit code) is a crash.
#[cfg(unix)]
fn is_crash(status: std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    status.signal().is_some()
}

#[cfg(not(unix))]
fn is_crash(_status: std::process::ExitStatus) -> bool {
    false
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    match Cli::parse().command {
        Command::Engines => {
            for engine in all_engines() {
                match engine.probe() {
                    Ok(info) => println!("{:<12} {}", engine.id(), info.version),
                    Err(e) => println!("{:<12} not available ({e})", engine.id()),
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Fetch { corpus, ids } => {
            let manifest = Manifest::load(&corpus)?;
            let cache = corpus::cache_dir();
            let vectors: Vec<_> = if ids.is_empty() {
                manifest.vector.iter().collect()
            } else {
                ids.iter()
                    .map(|id| manifest.get(id))
                    .collect::<Result<_, _>>()?
            };
            for vector in vectors {
                let path = corpus::fetch(vector, &cache).await?;
                println!("{:<40} {}", vector.id, path.display());
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::CorpusImport {
            fluster,
            out,
            limit,
        } => {
            let mut imported = if fluster.is_dir() {
                calliope_core::fluster::import_dir(&fluster)?
            } else {
                calliope_core::fluster::import_suite_json(&std::fs::read_to_string(&fluster)?)?
            };
            if limit > 0 {
                imported.truncate(limit);
            }
            // merge additively into an existing manifest, keyed by id
            let mut manifest = if out.exists() {
                Manifest::load(&out)?
            } else {
                Manifest::default()
            };
            let existing: std::collections::HashSet<String> =
                manifest.vector.iter().map(|v| v.id.clone()).collect();
            let added = imported
                .iter()
                .filter(|v| !existing.contains(&v.id))
                .count();
            manifest
                .vector
                .extend(imported.into_iter().filter(|v| !existing.contains(&v.id)));
            manifest.vector.sort_by(|a, b| a.id.cmp(&b.id));
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&out, toml::to_string(&manifest)?)?;
            println!(
                "imported {added} new vectors ({} total) -> {}",
                manifest.vector.len(),
                out.display()
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Minimize {
            engine,
            input,
            out,
            timeout_secs,
            workdir,
        } => {
            let engine = engine_by_id(&engine)?;
            std::fs::create_dir_all(&workdir)?;
            let timeout = std::time::Duration::from_secs(timeout_secs);
            let candidate_path = workdir.join("candidate");

            let mut fails = |bytes: &[u8]| -> bool {
                std::fs::write(&candidate_path, bytes).is_ok()
                    && engine_crashes(engine.as_ref(), &candidate_path, &workdir, timeout)
            };

            let initial = std::fs::read(&input)?;
            if !fails(&initial) {
                bail!(
                    "{} does not crash or hang on {} (nothing to minimize)",
                    engine.id(),
                    input.display()
                );
            }
            let minimized = calliope_core::minimize::minimize(&initial, &mut fails);
            let out = out.unwrap_or_else(|| input.with_extension("min"));
            std::fs::write(&out, &minimized)?;
            println!(
                "minimized {} bytes -> {} bytes, reproducer: {}",
                initial.len(),
                minimized.len(),
                out.display()
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Run {
            scenarios,
            corpus,
            engines,
            jobs,
            workdir,
            report,
        } => {
            if scenarios.is_empty() {
                bail!("no scenario files given");
            }
            let loaded = scenarios
                .iter()
                .map(|p| Scenario::load(p).map_err(Into::into))
                .collect::<Result<Vec<_>>>()?;
            let out = run_matrix(&loaded, &corpus, &engines, jobs, &workdir).await?;
            print_summary(&out);
            if let Some(path) = report {
                std::fs::write(&path, serde_json::to_vec_pretty(&out)?)?;
                println!("report: {}", path.display());
            }
            Ok(if out.passed() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Command::Conformance {
            corpus,
            engines,
            limit,
            jobs,
            workdir,
            report,
        } => {
            let manifest = Manifest::load(&corpus)?;
            let engines = if engines.is_empty() {
                vec!["ffmpeg".into(), "gstreamer".into(), "g2g".into()]
            } else {
                engines
            };
            // one golden scenario per vector that carries a conformance hash
            let mut scenarios = golden_scenarios(&manifest.vector, &engines);
            if scenarios.is_empty() {
                bail!(
                    "no vectors in {} have both decoded-md5 and output-format (import a Fluster suite first)",
                    corpus.display()
                );
            }
            if limit > 0 {
                scenarios.truncate(limit);
            }
            println!(
                "conformance: {} vectors x {} engines",
                scenarios.len(),
                engines.len()
            );
            let out = run_matrix(&scenarios, &corpus, &[], jobs, &workdir).await?;
            print_summary(&out);
            let passed = out.scenarios.iter().filter(|s| s.passed()).count();
            println!(
                "conformance: {passed}/{} vectors passed",
                out.scenarios.len()
            );
            if let Some(path) = report {
                std::fs::write(&path, serde_json::to_vec_pretty(&out)?)?;
                println!("report: {}", path.display());
            }
            Ok(if out.passed() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

async fn run_matrix(
    scenarios: &[Scenario],
    corpus_path: &Path,
    engine_filter: &[String],
    jobs: usize,
    workdir: &Path,
) -> Result<Report> {
    // corpus manifest is optional when every scenario uses input.path
    let needs_corpus = scenarios.iter().any(|s| s.input.corpus.is_some());
    let manifest = if needs_corpus {
        Some(Manifest::load(corpus_path)?)
    } else {
        None
    };
    let cache = corpus::cache_dir();

    // resolve each scenario once (fetch input, corrupt for a fault, probe
    // geometry when a differential scenario omits [video]); the resolved
    // scenario is shared across its engines and drives the report.
    let mut resolved: Vec<(Arc<Scenario>, Option<String>, Option<u64>)> = Vec::new();
    let mut prepared: Vec<(Arc<Scenario>, Arc<dyn Engine>, PathBuf)> = Vec::new();
    // vectors whose input could not be fetched (dead URL, missing archive
    // member); skipped so one bad download does not abort the whole matrix
    let mut skipped: Vec<(String, String)> = Vec::new();
    for scenario in scenarios {
        // an encode scenario has no input to fetch: ffmpeg generates the
        // differential input by encoding a lavfi source, then every engine
        // decodes that stream and the frames are compared bit-exact.
        let input = if let Some(enc) = &scenario.encode {
            let video = scenario
                .video
                .expect("encode scenario has [video] (validated)");
            let dir = workdir.join(&scenario.id);
            std::fs::create_dir_all(&dir)?;
            let out = dir.join(format!("encoded.{}", enc.output_ext));
            calliope_adapter_ffmpeg::encode_source(
                &enc.source,
                &enc.encoder,
                &enc.args,
                video,
                &out,
            )
            .with_context(|| format!("{}: generating encode-differential input", scenario.id))?;
            out
        } else {
            let source = match (&scenario.input.corpus, &scenario.input.path) {
                (Some(id), _) => {
                    let vector = manifest.as_ref().unwrap().get(id)?;
                    match corpus::fetch(vector, &cache).await {
                        Ok(path) => path,
                        Err(e) => {
                            eprintln!("  skip {}: fetch failed: {e}", scenario.id);
                            skipped.push((scenario.id.clone(), e.to_string()));
                            continue;
                        }
                    }
                }
                (None, Some(path)) => path.clone(),
                _ => unreachable!("validated at load"),
            };
            // a robustness scenario corrupts the input once, then feeds the
            // mangled file to every engine (all engines see identical corruption)
            match &scenario.fault {
                Some(fault) => {
                    let dir = workdir.join(&scenario.id);
                    std::fs::create_dir_all(&dir)?;
                    let corrupted = dir.join("input.corrupted");
                    fault.corrupt_file(&source, &corrupted)?;
                    corrupted
                }
                None => source,
            }
        };
        let mut scenario = scenario.clone();
        let mut golden_expected = None;
        let mut reschange_expected = None;
        if scenario.is_resolution_change() {
            // ffprobe reports the true per-frame geometry across the change
            // (ffmpeg's CLI can't, it normalizes output size). Sum the packed
            // per-frame sizes: that byte total is what a correct decode emits.
            let frames = calliope_core::probe::probe_frame_geometry(&input)
                .with_context(|| format!("{}: probing per-frame geometry", scenario.id))?;
            reschange_expected = Some(frames.iter().map(|v| v.frame_size() as u64).sum());
            // pin the (constant) pixel format for the raw-dump engines; the
            // width / height vary per frame and stay unset in the caps
            scenario.video = Some(Video {
                width: 0,
                height: 0,
                format: frames[0].format,
            });
        } else if scenario.is_golden() {
            // the corpus vector carries the oracle: its output format and the
            // conformance MD5 every engine's decoded output must match
            let id = scenario.input.corpus.as_ref().expect("golden needs corpus");
            let vector = manifest.as_ref().unwrap().get(id)?;
            let format = vector.output_format.with_context(|| {
                format!(
                    "{}: vector '{id}' has no output-format for golden",
                    scenario.id
                )
            })?;
            golden_expected = Some(vector.decoded_md5.clone().with_context(|| {
                format!(
                    "{}: vector '{id}' has no decoded-md5 for golden",
                    scenario.id
                )
            })?);
            // golden hashes the whole output, so geometry is unused; only the
            // pixel format drives the engines' conversion target
            scenario.video = Some(Video {
                width: 0,
                height: 0,
                format,
            });
        } else if (scenario.judges_frames() || scenario.is_roundtrip())
            && scenario.video.is_none()
            && !scenario.is_audio()
        {
            // differential chunks by geometry; roundtrip needs it to size the raw
            // PSNR compare. Audio has no video geometry, so it skips the probe.
            scenario.video = Some(
                calliope_core::probe::probe_geometry(&input)
                    .with_context(|| format!("{}: auto-probing geometry", scenario.id))?,
            );
        }
        let scenario = Arc::new(scenario);
        let engine_ids: Vec<&String> = scenario
            .engines
            .iter()
            .filter(|id| engine_filter.is_empty() || engine_filter.contains(id))
            .collect();
        if !engine_ids.iter().any(|id| **id == scenario.reference) {
            bail!(
                "{}: --engines filtered out reference '{}'",
                scenario.id,
                scenario.reference
            );
        }
        for id in engine_ids {
            prepared.push((Arc::clone(&scenario), engine_by_id(id)?, input.clone()));
        }
        resolved.push((scenario, golden_expected, reschange_expected));
    }
    if !skipped.is_empty() {
        eprintln!("skipped {} vector(s) that failed to fetch", skipped.len());
    }

    // flat (scenario, engine) matrix with bounded parallelism
    let mut pending = FuturesUnordered::new();
    let mut prepared = prepared.into_iter();
    let mut results: Vec<(String, calliope_core::runner::RunResult)> = Vec::new();
    loop {
        while pending.len() < jobs {
            let Some((scenario, engine, input)) = prepared.next() else {
                break;
            };
            pending.push(async move {
                let result = if scenario.is_soak() {
                    run_soak(engine.as_ref(), &scenario, &input, workdir).await
                } else if scenario.is_determinism() {
                    run_determinism(engine.as_ref(), &scenario, &input, workdir).await
                } else if scenario.is_roundtrip() {
                    // the engine transcodes (decode -> re-encode); ffmpeg then
                    // decodes that stream and PSNR-compares it to the reference.
                    let mut r = run_one(engine.as_ref(), &scenario, &input, workdir).await;
                    if matches!(r.status, RunStatus::Ok)
                        && let (Some(rt), Some(video)) = (&scenario.roundtrip, scenario.video)
                    {
                        let encoded = r.log_dir.join(format!("out.{}", rt.output_ext));
                        let dir = r.log_dir.clone();
                        match calliope_adapter_ffmpeg::roundtrip_psnr(&input, &encoded, &dir, video)
                        {
                            Ok(psnr) => r.psnr = Some(psnr),
                            Err(e) => {
                                r.status = RunStatus::Error {
                                    message: format!("roundtrip validate: {e}"),
                                }
                            }
                        }
                    }
                    r
                } else {
                    run_one(engine.as_ref(), &scenario, &input, workdir).await
                };
                (scenario.id.clone(), result)
            });
        }
        match pending.next().await {
            Some(done) => results.push(done),
            None => break,
        }
    }

    let mut report = Report {
        scenarios: Vec::new(),
    };
    for (scenario, golden_expected, reschange_expected) in &resolved {
        let mut runs: Vec<_> = results
            .extract_if(.., |(id, _)| id == &scenario.id)
            .map(|(_, run)| run)
            .collect();
        // reference first, then scenario engine order, for stable reports
        runs.sort_by_key(|r| {
            (
                r.engine != scenario.reference,
                scenario.engines.iter().position(|e| *e == r.engine),
            )
        });
        // majority vote across every engine that produced frame hashes, so a
        // divergence names the outlier instead of blaming the reference's peers
        let voters: Vec<(String, Vec<String>)> = runs
            .iter()
            .filter_map(|r| r.frame_hashes.clone().map(|h| (r.engine.clone(), h)))
            .collect();
        let majority = (voters.len() >= 3).then(|| calliope_core::compare::majority_vote(&voters));
        let reference_hashes = runs
            .iter()
            .find(|r| r.engine == scenario.reference)
            .and_then(|r| r.frame_hashes.clone());
        let runs = runs
            .into_iter()
            .map(|run| {
                let comparison = match (&reference_hashes, &run.frame_hashes) {
                    (Some(reference), Some(candidate)) if run.engine != scenario.reference => {
                        Some(compare(reference, candidate))
                    }
                    _ => None,
                };
                EngineReport { run, comparison }
            })
            .collect();
        report.scenarios.push(ScenarioReport {
            scenario: scenario.id.clone(),
            reference: scenario.reference.clone(),
            robustness: scenario.is_robustness(),
            soak: scenario.is_soak(),
            determinism: scenario.is_determinism(),
            outcome_diff: scenario.is_outcome_diff(),
            golden_expected: golden_expected.clone(),
            majority,
            roundtrip_psnr_min: scenario.roundtrip.as_ref().map(|rt| rt.psnr_min),
            resolution_change_expected_bytes: *reschange_expected,
            runs,
        });
    }
    Ok(report)
}

/// one-word label for the reference engine's own decode outcome
fn outcome_label(outcome: DecodeOutcome) -> &'static str {
    match outcome {
        DecodeOutcome::Decoded => "decoded",
        DecodeOutcome::Rejected => "rejected",
        DecodeOutcome::Crashed => "CRASHED",
        DecodeOutcome::Hung => "HUNG",
        DecodeOutcome::HarnessError => "error",
    }
}

fn print_summary(report: &Report) {
    for scenario in &report.scenarios {
        println!("{}", scenario.scenario);
        for r in &scenario.runs {
            let status = match &r.run.status {
                RunStatus::Ok => "ok".to_string(),
                RunStatus::ExitFailure { code } => format!("exit {code}"),
                RunStatus::Signaled { signal } => format!("SIGNAL {signal}"),
                RunStatus::Timeout => "TIMEOUT".to_string(),
                RunStatus::Error { message } => format!("error: {message}"),
            };
            let frames = r.run.frame_hashes.as_ref().map_or(0, Vec::len);
            let rss = r
                .run
                .peak_rss_kb
                .map_or("-".into(), |kb| format!("{} MB", kb / 1024));
            let verdict = if let Some(min) = scenario.roundtrip_psnr_min {
                match (&r.run.status, r.run.psnr) {
                    (RunStatus::Ok, Some(p)) if p >= min => format!("roundtrip ok ({p:.1} dB)"),
                    (RunStatus::Ok, Some(p)) => format!("PSNR {p:.1} dB < {min}"),
                    (RunStatus::Signaled { signal }, _) => format!("CRASHED (signal {signal})"),
                    (RunStatus::Timeout, _) => "HUNG".to_string(),
                    _ => "encode / validate failed".to_string(),
                }
            } else if let Some(expected) = scenario.resolution_change_expected_bytes {
                match (&r.run.status, r.run.output_len) {
                    (RunStatus::Ok, Some(got)) if got == expected => {
                        format!("res-change ok ({got} bytes)")
                    }
                    (RunStatus::Ok, Some(got)) => {
                        format!("WRONG OUTPUT {got} vs {expected} bytes")
                    }
                    (RunStatus::Signaled { signal }, _) => format!("CRASHED (signal {signal})"),
                    (RunStatus::Timeout, _) => "HUNG".to_string(),
                    _ => "no output".to_string(),
                }
            } else if let Some(expected) = &scenario.golden_expected {
                match &r.run.output_md5 {
                    Some(got) if got.eq_ignore_ascii_case(expected) => "golden ok".to_string(),
                    Some(got) => format!("GOLDEN MISMATCH ({}…)", &got[..got.len().min(8)]),
                    None => "no output".to_string(),
                }
            } else if scenario.determinism {
                let runs = r.run.iterations_completed.unwrap_or(0);
                match r.run.determinism_matched {
                    Some(true) => format!("deterministic ({runs} runs)"),
                    Some(false) => match &r.run.status {
                        RunStatus::Signaled { signal } => format!("CRASHED (signal {signal})"),
                        RunStatus::Timeout => "HUNG".to_string(),
                        _ => "NONDETERMINISTIC".to_string(),
                    },
                    None => "no output".to_string(),
                }
            } else if scenario.soak {
                let done = r.run.iterations_completed.unwrap_or(0);
                match &r.run.status {
                    RunStatus::Signaled { signal } => {
                        format!("CRASHED (signal {signal}) at run {done}")
                    }
                    RunStatus::Timeout => format!("HUNG at run {done}"),
                    s if s.survived_corrupt_input() => format!("stable ({done} runs)"),
                    _ => format!("errored at run {done}"),
                }
            } else if scenario.outcome_diff {
                // corrupt-input differential: the reference row shows its own
                // outcome; each candidate is judged against it. A crash / hang
                // still fails the run (robustness bar); the decode-outcome
                // splits are advisory, with LENIENT (decoded what the reference
                // refused) the high-value one.
                let outcome = DecodeOutcome::of(&r.run);
                if r.run.engine == scenario.reference {
                    format!("reference: {}", outcome_label(outcome))
                } else {
                    let reference = scenario
                        .runs
                        .iter()
                        .find(|x| x.run.engine == scenario.reference)
                        .map(|x| DecodeOutcome::of(&x.run));
                    match reference.map(|refo| outcome_verdict(refo, outcome)) {
                        Some(OutcomeVerdict::Agree) => "agree".to_string(),
                        Some(OutcomeVerdict::Lenient) => {
                            format!("LENIENT: decoded input {} rejected", scenario.reference)
                        }
                        Some(OutcomeVerdict::Stricter) => {
                            format!("stricter: rejected input {} decoded", scenario.reference)
                        }
                        Some(OutcomeVerdict::Crashed) => "CRASHED".to_string(),
                        Some(OutcomeVerdict::Hung) => "HUNG".to_string(),
                        Some(OutcomeVerdict::Inconclusive) | None => "inconclusive".to_string(),
                    }
                }
            } else if scenario.robustness {
                // absolute per engine: a crash / hang is a hardening bug
                match &r.run.status {
                    RunStatus::Signaled { .. } => "CRASHED".to_string(),
                    RunStatus::Timeout => "HUNG".to_string(),
                    s if s.survived_corrupt_input() => "survived".to_string(),
                    _ => "errored".to_string(),
                }
            } else if let Some(vote) = scenario
                .majority
                .as_ref()
                .filter(|v| v.conclusive && !v.outliers.is_empty())
            {
                // a real divergence with a clear majority: name the culprit
                // (even when it is the reference) instead of blaming its peers
                if vote.is_outlier(&r.run.engine) {
                    let tag = if r.run.engine == scenario.reference {
                        " [reference]"
                    } else {
                        ""
                    };
                    format!("OUTLIER vs {}-engine majority{tag}", vote.majority.len())
                } else {
                    "majority ok".to_string()
                }
            } else if r.run.engine == scenario.reference {
                "(reference)".to_string()
            } else {
                match &r.comparison {
                    Some(c) if c.matched => "match".to_string(),
                    Some(c) => match c.first_divergence {
                        Some(i) => format!("DIVERGED at frame {i}"),
                        None => format!("FRAME COUNT {} vs {}", c.frames, c.reference_frames),
                    },
                    None => "no comparison".to_string(),
                }
            };
            println!(
                "  {:<12} {:<10} {:>5} frames  {:>6.1}s  {:>8}  {verdict}",
                r.run.engine,
                status,
                frames,
                r.run.duration_ms as f64 / 1000.0,
                rss,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use calliope_core::corpus::Vector;

    fn vector(id: &str, golden: bool) -> Vector {
        Vector {
            id: id.into(),
            url: "u".into(),
            sha256: Some("00".into()),
            md5: None,
            archive_member: None,
            decoded_md5: golden.then(|| "abc".into()),
            output_format: golden.then_some(calliope_core::scenario::PixelFormat::I420),
            license: "l".into(),
            notes: String::new(),
        }
    }

    #[test]
    fn golden_scenarios_only_for_vectors_with_a_conformance_hash() {
        let vectors = [vector("has-hash", true), vector("no-hash", false)];
        let engines = vec!["ffmpeg".to_string(), "g2g".to_string()];
        let scenarios = golden_scenarios(&vectors, &engines);
        assert_eq!(scenarios.len(), 1);
        let s = &scenarios[0];
        assert_eq!(s.id, "has-hash");
        assert!(s.is_golden());
        assert_eq!(s.reference, "ffmpeg");
        assert_eq!(s.input.corpus.as_deref(), Some("has-hash"));
    }
}
