use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use futures::stream::{FuturesUnordered, StreamExt};

use calliope_core::compare::compare;
use calliope_core::corpus::{self, Manifest};
use calliope_core::engine::Engine;
use calliope_core::report::{EngineReport, Report, ScenarioReport};
use calliope_core::runner::{RunStatus, run_one};
use calliope_core::scenario::Scenario;

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

    let mut prepared = Vec::new();
    for scenario in scenarios {
        let source = match (&scenario.input.corpus, &scenario.input.path) {
            (Some(id), _) => {
                let vector = manifest.as_ref().unwrap().get(id)?;
                corpus::fetch(vector, &cache).await?
            }
            (None, Some(path)) => path.clone(),
            _ => unreachable!("validated at load"),
        };
        // a robustness scenario corrupts the input once, then feeds the mangled
        // file to every engine (all engines see the identical corruption)
        let input = match &scenario.fault {
            Some(fault) => {
                let dir = workdir.join(&scenario.id);
                std::fs::create_dir_all(&dir)?;
                let corrupted = dir.join("input.corrupted");
                fault.corrupt_file(&source, &corrupted)?;
                corrupted
            }
            None => source,
        };
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
            prepared.push((scenario, engine_by_id(id)?, input.clone()));
        }
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
                let result = run_one(engine.as_ref(), scenario, &input, workdir).await;
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
    for scenario in scenarios {
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
            runs,
        });
    }
    Ok(report)
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
            let verdict = if scenario.robustness {
                // absolute per engine: a crash / hang is a hardening bug
                match &r.run.status {
                    RunStatus::Signaled { .. } => "CRASHED".to_string(),
                    RunStatus::Timeout => "HUNG".to_string(),
                    s if s.survived_corrupt_input() => "survived".to_string(),
                    _ => "errored".to_string(),
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
