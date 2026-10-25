mod checks;
mod config;
mod record;
mod schedule;
mod status;
mod supervisor;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail, ensure};
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use calliope_core::runner::read_proc_status_kb;

use checks::{FrameChecks, MEMORY_GROWTH_LIMIT_KIB_PER_HOUR, RssWindow};
use config::{CanaryConfig, Tier};
use record::{
    Cause, INCIDENTS_PREFIX, IncidentKind, OpenIncidents, RECOVERIES_PREFIX, Recovery,
    SAMPLES_PREFIX, SECS_PER_DAY, STDERR_DIR, Sample, SharedOpenIncidents, unix_now, until_unix,
};
use schedule::RecoveryWindow;
use supervisor::{PipelineState, SharedState, Supervisor, UnrecoveredRun};

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
    let mut unrecovered = Vec::with_capacity(states.len());
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
            let (sender, receiver) = unbounded_channel();
            unrecovered.push(sender);
            tokio::spawn(supervisor.run(receiver))
        })
        .collect();
    let mut sampler = Sampler::new(&config, g2g_commit, states, open_incidents, unrecovered);
    let mut ticker = tokio::time::interval(Duration::from_secs(config.sample_secs));
    // the first tick fires at once, with nothing to sample yet
    ticker.tick().await;
    sampler.write_status()?;
    let fault_schedule = schedule::fault_schedule();
    let started = unix_now();
    let mut recovery_window = fault_schedule.next_recovery_window(started);
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        let until_judged = until_unix(recovery_window.end, SystemTime::now());
        tokio::select! {
            _ = ticker.tick() => {
                sampler.sample()?;
                sampler.write_status()?;
            }
            _ = tokio::time::sleep(until_judged) => {
                // the canary started inside this window, after its fault
                if recovery_window.start >= started {
                    sampler.judge_recovery(&recovery_window)?;
                }
                recovery_window = fault_schedule.next_recovery_window(recovery_window.end);
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
    // to each pipeline's supervisor
    unrecovered: Vec<UnboundedSender<UnrecoveredRun>>,
}

impl<'a> Sampler<'a> {
    fn new(
        config: &'a CanaryConfig,
        g2g_commit: String,
        states: Vec<SharedState>,
        open_incidents: SharedOpenIncidents,
        unrecovered: Vec<UnboundedSender<UnrecoveredRun>>,
    ) -> Self {
        Sampler {
            config,
            g2g_commit,
            sampled_frames: vec![0; states.len()],
            rss_windows: states.iter().map(|_| RssWindow::default()).collect(),
            states,
            open_incidents,
            sampled_at: Instant::now(),
            unrecovered,
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
                .filter(|counts| counts.faults(pipeline.tier) > 0)
                .map(|counts| Cause::Content {
                    mismatched: counts.mismatched,
                    skipped: counts.skipped,
                }),
            content.is_some_and(|counts| counts.matched > 0 && counts.faults(pipeline.tier) == 0),
            sample.ts,
        )?;
        let timestamps = sample.timestamps;
        open_incidents.track(
            pipeline,
            IncidentKind::Timestamps,
            timestamps
                .filter(|counts| counts.faults(pipeline.tier) > 0)
                .map(|counts| Cause::Timestamps {
                    backward: counts.backward,
                    forward_jumps: counts.forward_jumps,
                }),
            timestamps.is_some_and(|counts| counts.faults(pipeline.tier) == 0),
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

    // records how each faulted row came back from the fault before `window`, and has a run that never did restarted
    fn judge_recovery(&self, window: &RecoveryWindow) -> Result<()> {
        for (index, pipeline) in self.config.pipelines.iter().enumerate() {
            if pipeline.tier != Tier::Faulted {
                continue;
            }
            let (recovery, pid) = {
                let mut state = self.states[index].lock().expect("pipeline state lock");
                let recovery = state
                    .checks
                    .take_recovery(window.start as f64, window.end as f64);
                (recovery, state.pid)
            };
            let record = Recovery {
                pipeline: pipeline.id.clone(),
                fault: window.fault.name.to_string(),
                fault_end: window.start,
                window_end: window.end,
                recovery_secs: recovery.recovery_secs,
                av_drift_ms: recovery.av_drift_ms,
            };
            record::append(&self.config.log_dir, RECOVERIES_PREFIX, window.end, &record)?;
            if recovery.recovery_secs.is_some() {
                self.open_incidents
                    .lock()
                    .expect("open incidents lock")
                    .close(&pipeline.id, IncidentKind::NoRecovery, window.end)?;
                continue;
            }
            // a row with no run is already down, and its supervisor opened that incident
            let Some(pid) = pid else {
                continue;
            };
            self.unrecovered[index]
                .send(UnrecoveredRun {
                    pid,
                    fault: record.fault,
                })
                .map_err(|_| anyhow!("pipeline '{}' supervisor stopped", pipeline.id))?;
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
        incidents.extend_from_slice(
            self.open_incidents
                .lock()
                .expect("open incidents lock")
                .all(),
        );
        let week_ago = now.saturating_sub(status::WEEK_DAYS * SECS_PER_DAY);
        let recoveries: Vec<Recovery> =
            record::read_days(&self.config.log_dir, RECOVERIES_PREFIX, week_ago, now)?;
        let summary = status::summarize(
            self.config,
            &samples,
            &incidents,
            &recoveries,
            &self.g2g_commit,
            now,
        );
        status::write_status(&self.config.status_dir, &summary)
    }
}
