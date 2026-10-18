mod config;
mod record;
mod status;
mod supervisor;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::stream::{FuturesUnordered, StreamExt};

use calliope_core::runner::read_proc_status_kb;

use config::CanaryConfig;
use record::{INCIDENTS_PREFIX, SAMPLES_PREFIX, SECS_PER_DAY, STDERR_DIR, Sample, unix_now};
use supervisor::{PipelineState, SharedState, Supervisor};

const NANOS_PER_MILLI: f64 = 1_000_000.0;

pub async fn run(config_path: &Path) -> Result<()> {
    let config = CanaryConfig::load(config_path)?;
    let stderr_dir = config.log_dir.join(STDERR_DIR);
    std::fs::create_dir_all(&stderr_dir)
        .with_context(|| format!("create {}", stderr_dir.display()))?;
    let states: Vec<SharedState> = config
        .pipelines
        .iter()
        .map(|_| Arc::new(Mutex::new(PipelineState::default())))
        .collect();
    let mut supervisors: FuturesUnordered<_> = states
        .iter()
        .enumerate()
        .map(|(index, state)| {
            tokio::spawn(Supervisor::new(&config, index, Arc::clone(state)).run())
        })
        .collect();
    let mut sampler = Sampler::new(&config, states);
    let mut ticker = tokio::time::interval(Duration::from_secs(config.sample_secs));
    // the first tick fires at once, with nothing to sample yet
    ticker.tick().await;
    sampler.write_status()?;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                sampler.sample()?;
                sampler.write_status()?;
            }
            ended = supervisors.next() => {
                match ended {
                    Some(Ok(Err(e))) => return Err(e),
                    Some(Err(e)) => bail!("pipeline supervisor panicked: {e}"),
                    _ => bail!("pipeline supervisor stopped"),
                }
            }
            signal = &mut shutdown => return signal,
        }
    }
}

#[cfg(unix)]
async fn shutdown_signal() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
    Ok(())
}

#[cfg(not(unix))]
async fn shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c().await.context("wait for ctrl-c")
}

#[derive(Debug)]
struct Sampler<'a> {
    config: &'a CanaryConfig,
    states: Vec<SharedState>,
    // frames_total of each pipeline at the previous sample
    sampled_frames: Vec<u64>,
    sampled_at: Instant,
}

impl<'a> Sampler<'a> {
    fn new(config: &'a CanaryConfig, states: Vec<SharedState>) -> Self {
        Sampler {
            config,
            sampled_frames: vec![0; states.len()],
            states,
            sampled_at: Instant::now(),
        }
    }

    fn sample(&mut self) -> Result<()> {
        let now = unix_now();
        let elapsed = self.sampled_at.elapsed().as_secs_f64();
        self.sampled_at = Instant::now();
        for (index, pipeline) in self.config.pipelines.iter().enumerate() {
            let state = self.states[index].lock().expect("pipeline state lock");
            let frames = state.frames_total - self.sampled_frames[index];
            self.sampled_frames[index] = state.frames_total;
            let sample = Sample {
                ts: now,
                pipeline: pipeline.id.clone(),
                tier: pipeline.tier,
                alive: state.pid.is_some(),
                frames,
                fps: frames as f64 / elapsed,
                decode_latency_ms: state
                    .decode_latency_ns
                    .map(|ns| ns as f64 / NANOS_PER_MILLI),
                rss_kb: state.pid.and_then(|pid| read_proc_status_kb(pid, "VmRSS:")),
                restarts: state.restarts,
                last_error: state.last_error.clone(),
            };
            record::append(&self.config.log_dir, SAMPLES_PREFIX, now, &sample)?;
        }
        Ok(())
    }

    fn write_status(&self) -> Result<()> {
        let now = unix_now();
        let since = now.saturating_sub(status::HISTORY_DAYS * SECS_PER_DAY);
        let samples: Vec<Sample> =
            record::read_days(&self.config.log_dir, SAMPLES_PREFIX, since, now)?;
        let mut incidents: Vec<record::Incident> =
            record::read_days(&self.config.log_dir, INCIDENTS_PREFIX, since, now)?;
        incidents.extend(self.states.iter().filter_map(|state| {
            state
                .lock()
                .expect("pipeline state lock")
                .open_incident
                .clone()
        }));
        let summary = status::summarize(&self.config.pipelines, &samples, &incidents, now);
        status::write_status(&self.config.status_dir, &summary)
    }
}
