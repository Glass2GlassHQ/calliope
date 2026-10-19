mod checks;
mod config;
mod record;
mod status;
mod supervisor;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use futures::stream::{FuturesUnordered, StreamExt};

use calliope_core::runner::read_proc_status_kb;

use checks::{FrameChecks, MEMORY_GROWTH_LIMIT_KIB_PER_HOUR, RssWindow};
use config::CanaryConfig;
use record::{
    Cause, INCIDENTS_PREFIX, IncidentKind, OpenIncidents, SAMPLES_PREFIX, SECS_PER_DAY, STDERR_DIR,
    Sample, SharedOpenIncidents, unix_now,
};
use supervisor::{PipelineState, SharedState, Supervisor};

const NANOS_PER_MILLI: f64 = 1_000_000.0;

pub async fn run(config_path: &Path) -> Result<()> {
    let g2g_commit = std::env::var("G2G_COMMIT")
        .context("G2G_COMMIT must name the g2g commit g2g-launch was built from")?;
    ensure!(!g2g_commit.is_empty(), "G2G_COMMIT is empty");
    let config = CanaryConfig::load(config_path)?;
    let stderr_dir = config.log_dir.join(STDERR_DIR);
    std::fs::create_dir_all(&stderr_dir)
        .with_context(|| format!("create {}", stderr_dir.display()))?;
    let states = config
        .pipelines
        .iter()
        .map(|pipeline| {
            let state = PipelineState::new(FrameChecks::new(pipeline)?);
            Ok(Arc::new(Mutex::new(state)))
        })
        .collect::<Result<Vec<SharedState>>>()?;
    let mut open_incidents = OpenIncidents::load(&config.log_dir)?;
    open_incidents.close_unconfigured(&config.pipelines, unix_now())?;
    let open_incidents = Arc::new(Mutex::new(open_incidents));
    let mut supervisors: FuturesUnordered<_> = states
        .iter()
        .enumerate()
        .map(|(index, state)| {
            let supervisor = Supervisor::new(
                &config,
                index,
                Arc::clone(state),
                Arc::clone(&open_incidents),
            );
            tokio::spawn(supervisor.run())
        })
        .collect();
    let mut sampler = Sampler::new(&config, g2g_commit, states, open_incidents);
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
    g2g_commit: String,
    states: Vec<SharedState>,
    open_incidents: SharedOpenIncidents,
    // frames_total of each pipeline at the previous sample
    sampled_frames: Vec<u64>,
    rss_windows: Vec<RssWindow>,
    sampled_at: Instant,
}

impl<'a> Sampler<'a> {
    fn new(
        config: &'a CanaryConfig,
        g2g_commit: String,
        states: Vec<SharedState>,
        open_incidents: SharedOpenIncidents,
    ) -> Self {
        Sampler {
            config,
            g2g_commit,
            sampled_frames: vec![0; states.len()],
            rss_windows: states.iter().map(|_| RssWindow::default()).collect(),
            states,
            open_incidents,
            sampled_at: Instant::now(),
        }
    }

    fn sample(&mut self) -> Result<()> {
        let now = unix_now();
        let elapsed = self.sampled_at.elapsed().as_secs_f64();
        self.sampled_at = Instant::now();
        for (index, pipeline) in self.config.pipelines.iter().enumerate() {
            let sample = {
                let mut state = self.states[index].lock().expect("pipeline state lock");
                let frames = state.frames_total - self.sampled_frames[index];
                self.sampled_frames[index] = state.frames_total;
                let rss_kb = state.pid.and_then(|pid| read_proc_status_kb(pid, "VmRSS:"));
                let rss_growth_kib_per_hour = state
                    .pid
                    .zip(rss_kb)
                    .and_then(|(pid, kb)| self.rss_windows[index].push(pid, now, kb));
                let frame_sample = state.checks.take_sample();
                Sample {
                    ts: now,
                    pipeline: pipeline.id.clone(),
                    tier: pipeline.tier,
                    alive: state.pid.is_some(),
                    frames,
                    fps: frames as f64 / elapsed,
                    decode_latency_ms: state
                        .decode_latency_ns
                        .map(|ns| ns as f64 / NANOS_PER_MILLI),
                    rss_kb,
                    restarts: state.restarts,
                    last_error: state.last_error.clone(),
                    content: frame_sample.content,
                    timestamps: frame_sample.timestamps,
                    av_drift_ms: frame_sample.av_drift_ms,
                    rss_growth_kib_per_hour,
                }
            };
            record::append(&self.config.log_dir, SAMPLES_PREFIX, now, &sample)?;
            self.judge(pipeline, &sample)?;
        }
        Ok(())
    }

    // opens, extends or closes the incidents a sample's checks decide
    fn judge(&self, pipeline: &config::PipelineConfig, sample: &Sample) -> Result<()> {
        let mut open_incidents = self.open_incidents.lock().expect("open incidents lock");
        let content = sample.content;
        open_incidents.track(
            pipeline,
            IncidentKind::Content,
            content
                .filter(|counts| counts.bad() > 0)
                .map(|counts| Cause::Content {
                    mismatched: counts.mismatched,
                    skipped: counts.skipped,
                }),
            content.is_some_and(|counts| counts.matched > 0 && counts.bad() == 0),
            sample.ts,
        )?;
        let timestamps = sample.timestamps;
        open_incidents.track(
            pipeline,
            IncidentKind::Timestamps,
            timestamps
                .filter(|counts| counts.bad() > 0)
                .map(|counts| Cause::Timestamps {
                    backward: counts.backward,
                    forward_jumps: counts.forward_jumps,
                }),
            timestamps.is_some_and(|counts| counts.bad() == 0),
            sample.ts,
        )?;
        let growth = sample.rss_growth_kib_per_hour;
        open_incidents.track(
            pipeline,
            IncidentKind::MemoryGrowth,
            growth
                .filter(|&kib_per_hour| kib_per_hour > MEMORY_GROWTH_LIMIT_KIB_PER_HOUR)
                .map(|kib_per_hour| Cause::MemoryGrowth { kib_per_hour }),
            growth.is_some_and(|kib_per_hour| kib_per_hour <= MEMORY_GROWTH_LIMIT_KIB_PER_HOUR),
            sample.ts,
        )
    }

    fn write_status(&self) -> Result<()> {
        let now = unix_now();
        let since = now.saturating_sub(status::HISTORY_DAYS * SECS_PER_DAY);
        let samples: Vec<Sample> =
            record::read_days(&self.config.log_dir, SAMPLES_PREFIX, since, now)?;
        let mut incidents: Vec<record::Incident> =
            record::read_days(&self.config.log_dir, INCIDENTS_PREFIX, since, now)?;
        incidents.extend_from_slice(
            self.open_incidents
                .lock()
                .expect("open incidents lock")
                .all(),
        );
        let summary = status::summarize(
            &self.config.pipelines,
            &samples,
            &incidents,
            &self.g2g_commit,
            now,
        );
        status::write_status(&self.config.status_dir, &summary)
    }
}
