use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc::UnboundedReceiver;

use calliope_adapter_g2g::observe::{ObserveMessage, follow_observe, observe_args};
use calliope_core::runner::{RunStatus, exit_to_status};

use super::checks::FrameChecks;
use super::config::{CanaryConfig, PipelineConfig, Tier};
use super::record::{
    Cause, Incident, IncidentKind, STDERR_DIR, SharedOpenIncidents, unix_now, unix_now_fractional,
};

pub const STDERR_TAIL_LINES: usize = 50;
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(1);
// stderr reaches EOF as soon as the process is gone
const STDERR_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Default)]
pub struct PipelineState {
    pub pid: Option<u32>,
    // frames into the sinks over every run of this pipeline
    pub frames_total: u64,
    pub decode_latency_ns: Option<u64>,
    pub restarts: u64,
    pub last_error: Option<String>,
    pub checks: FrameChecks,
    // the current run's last frame, as (monotonic, unix seconds)
    last_frame: Option<(Instant, u64)>,
}

impl PipelineState {
    pub fn new(checks: FrameChecks) -> Self {
        PipelineState {
            checks,
            ..PipelineState::default()
        }
    }
}

pub type SharedState = Arc<Mutex<PipelineState>>;

// a faulted row's run that did not recover from `fault`, for its supervisor to kill
#[derive(Debug)]
pub struct UnrecoveredRun {
    pub pid: u32,
    pub fault: String,
}

#[derive(Debug)]
pub struct Supervisor {
    pipeline: PipelineConfig,
    program: String,
    port: u16,
    log_dir: PathBuf,
    stall: Duration,
    backoff_min: Duration,
    backoff_max: Duration,
    healthy_run: Duration,
    startup_grace_ends: Instant,
    state: SharedState,
    open_incidents: SharedOpenIncidents,
}

#[derive(Debug)]
struct RunEnd {
    cause: Cause,
    // unix seconds the run stopped decoding
    failed_at: u64,
    decoded: bool,
    ran_for: Duration,
    stderr_tail: Vec<String>,
}

impl Supervisor {
    pub fn new(
        config: &CanaryConfig,
        index: usize,
        state: SharedState,
        open_incidents: SharedOpenIncidents,
    ) -> Self {
        Supervisor {
            pipeline: config.pipelines[index].clone(),
            program: config
                .g2g_launch
                .clone()
                .unwrap_or_else(calliope_adapter_g2g::g2g_launch_binary),
            port: config.observe_port(index),
            log_dir: config.log_dir.clone(),
            stall: Duration::from_secs(config.stall_secs),
            backoff_min: Duration::from_secs(config.backoff_min_secs),
            backoff_max: Duration::from_secs(config.backoff_max_secs),
            healthy_run: Duration::from_secs(config.healthy_run_secs),
            startup_grace_ends: Instant::now() + Duration::from_secs(config.startup_grace_secs),
            state,
            open_incidents,
        }
    }

    // returns only on a log write error
    pub async fn run(self, mut unrecovered: UnboundedReceiver<UnrecoveredRun>) -> Result<()> {
        let mut backoff = self.backoff_min;
        loop {
            let end = self.run_once(&mut unrecovered).await?;
            if end.ran_for >= self.healthy_run {
                backoff = self.backoff_min;
            }
            let ended_cleanly =
                self.pipeline.ends && end.decoded && end.cause == Cause::Exit { code: 0 };
            if ended_cleanly {
                continue;
            }
            self.record_failure(end)?;
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(self.backoff_max);
        }
    }

    async fn run_once(
        &self,
        unrecovered: &mut UnboundedReceiver<UnrecoveredRun>,
    ) -> Result<RunEnd> {
        let spawned_at = Instant::now();
        let spawned_unix = unix_now();
        let mut child = match Command::new(&self.program)
            .args(observe_args(&self.pipeline.launch, self.port))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                return Ok(RunEnd {
                    cause: Cause::SpawnFailed {
                        message: format!("{}: {e}", self.program),
                    },
                    failed_at: spawned_unix,
                    decoded: false,
                    ran_for: Duration::ZERO,
                    stderr_tail: Vec::new(),
                });
            }
        };
        {
            let mut state = self.lock();
            state.pid = child.id();
            state.last_frame = None;
            state.checks.restart();
        }
        let tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));
        let stderr_reader = child.stderr.take().map(|stderr| {
            tokio::spawn(read_stderr(
                stderr,
                Arc::clone(&tail),
                Arc::clone(&self.state),
            ))
        });
        let telemetry = tokio::spawn(follow_observe(self.port, self.on_message()));

        let watched = self
            .watch(&mut child, spawned_at, spawned_unix, unrecovered)
            .await;
        telemetry.abort();
        if let Some(reader) = stderr_reader {
            let _ = tokio::time::timeout(STDERR_DRAIN_TIMEOUT, reader).await;
        }
        let decoded = {
            let mut state = self.lock();
            state.pid = None;
            state.last_frame.is_some()
        };
        let (cause, failed_at) = watched?;
        let stderr_tail = tail.lock().expect("stderr tail lock").drain(..).collect();
        Ok(RunEnd {
            cause,
            failed_at,
            decoded,
            ran_for: spawned_at.elapsed(),
            stderr_tail,
        })
    }

    // waits for exit or kills on stall or no recovery, closing an open incident once frames flow
    async fn watch(
        &self,
        child: &mut Child,
        spawned_at: Instant,
        spawned_unix: u64,
        unrecovered: &mut UnboundedReceiver<UnrecoveredRun>,
    ) -> Result<(Cause, u64)> {
        let mut watchdog = tokio::time::interval(WATCHDOG_INTERVAL);
        // faults stall these rows on purpose, the recovery check judges them instead
        let stall_kills = self.pipeline.tier != Tier::Faulted;
        loop {
            tokio::select! {
                status = child.wait() => {
                    let status = status.context("wait for g2g-launch")?;
                    return Ok((exit_cause(status), unix_now()));
                }
                Some(run) = unrecovered.recv() => {
                    // a request for an earlier run comes too late
                    if child.id() == Some(run.pid) {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        return Ok((Cause::NoRecovery { fault: run.fault }, unix_now()));
                    }
                }
                _ = watchdog.tick() => {
                    let last_frame = self.lock().last_frame;
                    if last_frame.is_some() {
                        self.close_incident()?;
                    }
                    let (progress_at, progress_unix) =
                        last_frame.unwrap_or((spawned_at, spawned_unix));
                    if stall_kills && progress_at.elapsed() >= self.stall {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        return Ok((Cause::Stall { secs: self.stall.as_secs() }, progress_unix));
                    }
                }
            }
        }
    }

    fn on_message(&self) -> impl FnMut(ObserveMessage) + Send + 'static {
        let state = Arc::clone(&self.state);
        let mut seen_packets = 0;
        let run_started = Instant::now();
        move |message| {
            let mut state = state.lock().expect("pipeline state lock");
            match message {
                ObserveMessage::Telemetry(reading) => {
                    let new_packets = reading.sink_packets.saturating_sub(seen_packets);
                    seen_packets = reading.sink_packets;
                    state.decode_latency_ns = reading.decoder_p50_ns;
                    if new_packets > 0 {
                        state.frames_total += new_packets;
                        state.last_frame = Some((Instant::now(), unix_now()));
                    }
                }
                ObserveMessage::Checksum(reading) => {
                    let arrival_ns = run_started.elapsed().as_nanos() as i128;
                    state
                        .checks
                        .observe(&reading, arrival_ns, unix_now_fractional());
                }
            }
        }
    }

    fn record_failure(&self, end: RunEnd) -> Result<()> {
        let mut state = self.lock();
        state.restarts += 1;
        let mut open_incidents = self.open_incidents.lock().expect("open incidents lock");
        if let Some(open) = open_incidents.get(&self.pipeline.id, end.cause.kind()) {
            let mut incident = open.clone();
            incident.restarts += 1;
            return open_incidents.put(incident);
        }
        if Instant::now() < self.startup_grace_ends {
            return Ok(());
        }
        let id = format!("{}-{}", self.pipeline.id, end.failed_at);
        let stderr = format!("{STDERR_DIR}/{id}.log");
        let mut text = end.stderr_tail.join("\n");
        text.push('\n');
        let path = self.log_dir.join(&stderr);
        std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
        open_incidents.put(Incident {
            id,
            pipeline: self.pipeline.id.clone(),
            tier: self.pipeline.tier,
            start: end.failed_at,
            end: None,
            cause: end.cause,
            restarts: 1,
            last_error: state.last_error.clone(),
            stderr: Some(stderr),
        })
    }

    fn close_incident(&self) -> Result<()> {
        self.open_incidents
            .lock()
            .expect("open incidents lock")
            .close(&self.pipeline.id, IncidentKind::Down, unix_now())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PipelineState> {
        self.state.lock().expect("pipeline state lock")
    }
}

async fn read_stderr(
    stderr: tokio::process::ChildStderr,
    tail: Arc<Mutex<VecDeque<String>>>,
    state: SharedState,
) {
    let mut lines = BufReader::new(stderr).lines();
    let mut cause = None;
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        state.lock().expect("pipeline state lock").last_error =
            Some(shown_error(&line, &mut cause));
        let mut tail = tail.lock().expect("stderr tail lock");
        if tail.len() == STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }
}

// the stderr line an incident shows, with the last element error appended to the bare `pipeline error:` line
fn shown_error(line: &str, cause: &mut Option<String>) -> String {
    if let Some(error) = element_error(line) {
        *cause = Some(error);
    }
    match cause {
        Some(cause) if line.starts_with("pipeline error:") => format!("{line} ({cause})"),
        _ => line.to_string(),
    }
}

// the category and message of a g2g ERROR log line, except the runtime's restatement of the pipeline error
fn element_error(line: &str) -> Option<String> {
    let rest = line.strip_prefix("ERROR ")?;
    let (category, message) = rest.trim_start().split_once(char::is_whitespace)?;
    let message = message.trim_start();
    (!message.starts_with("pipeline error in ")).then(|| format!("{category}: {message}"))
}

fn exit_cause(status: ExitStatus) -> Cause {
    match exit_to_status(status) {
        RunStatus::Signaled { signal } => Cause::Signal { signal },
        RunStatus::ExitFailure { code } => Cause::Exit { code },
        RunStatus::Ok => Cause::Exit { code: 0 },
        other => unreachable!("an exit status never maps to {other:?}"),
    }
}

// fake g2g-launch scripts stand in for the subprocess, a local websocket stands in for its telemetry
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::canary::config::Tier;
    use crate::canary::record::{INCIDENTS_PREFIX, OpenIncidents, read_days};
    use futures::SinkExt;
    use std::path::Path;
    use tokio_tungstenite::tungstenite::Message;

    const STALL_SECS: u64 = 1;
    // long enough for a stall kill plus one restart at the minimum backoff
    const OBSERVE_FOR: Duration = Duration::from_millis(3_500);
    const TELEMETRY_INTERVAL: Duration = Duration::from_millis(100);
    const PACKETS_PER_TICK: u64 = 3;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("calliope-canary-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(STDERR_DIR)).unwrap();
        dir
    }

    // a child shell writes it, exec fails with ETXTBSY while a sibling test's fork holds our write handle
    fn fake_launch(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("fake-g2g-launch");
        let written = std::process::Command::new("sh")
            .args([
                "-c",
                r#"printf '#!/bin/sh\n%s\n' "$1" > "$2" && chmod 755 "$2""#,
                "sh",
            ])
            .arg(body)
            .arg(&path)
            .status()
            .unwrap();
        assert!(written.success(), "writing {} failed", path.display());
        path
    }

    fn config(dir: &Path, program: &Path, port: u16) -> CanaryConfig {
        config_with_grace(dir, program, port, 0)
    }

    fn config_with_grace(
        dir: &Path,
        program: &Path,
        port: u16,
        startup_grace_secs: u64,
    ) -> CanaryConfig {
        toml::from_str(&format!(
            r#"
            log-dir = "{dir}"
            status-dir = "{dir}/status"
            g2g-launch = "{program}"
            stall-secs = {STALL_SECS}
            observe-base-port = {port}
            backoff-min-secs = 1
            startup-grace-secs = {startup_grace_secs}
            [[pipeline]]
            id = "fake"
            tier = "self-hosted"
            launch = "videotestsrc ! fakesink"
            "#,
            dir = dir.display(),
            program = program.display(),
        ))
        .unwrap()
    }

    // the open incidents as a fresh canary process reads them back from disk
    fn reloaded_open_incidents(dir: &Path) -> Vec<Incident> {
        OpenIncidents::load(dir).unwrap().all().to_vec()
    }

    fn supervisor(config: &CanaryConfig, state: &SharedState) -> Supervisor {
        let open_incidents = Arc::new(Mutex::new(OpenIncidents::load(&config.log_dir).unwrap()));
        Supervisor::new(config, 0, Arc::clone(state), open_incidents)
    }

    async fn supervise_for(config: &CanaryConfig, state: &SharedState) {
        let (_unrecovered, requests) = tokio::sync::mpsc::unbounded_channel();
        let run = supervisor(config, state).run(requests);
        let ended = tokio::time::timeout(OBSERVE_FOR, run).await;
        assert!(ended.is_err(), "supervisor returned early: {ended:?}");
    }

    // bound for the whole test so no telemetry server gets its port, and never accepts
    fn silent_observe_listener() -> std::net::TcpListener {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap()
    }

    #[tokio::test]
    async fn kills_a_stalled_run_and_keeps_its_stderr() {
        let dir = scratch("stall");
        let program = fake_launch(&dir, "echo 'fake: waiting for stream' >&2\nexec sleep 60");
        let silent = silent_observe_listener();
        let config = config(&dir, &program, silent.local_addr().unwrap().port());
        let state = SharedState::default();
        supervise_for(&config, &state).await;

        let state = state.lock().unwrap();
        assert!(state.restarts >= 1);
        let open = reloaded_open_incidents(&dir);
        let incident = open.first().expect("stall opens an incident");
        assert_eq!(incident.cause, Cause::Stall { secs: STALL_SECS });
        assert_eq!(incident.tier, Tier::SelfHosted);
        let stderr = std::fs::read_to_string(dir.join(incident.stderr.as_ref().unwrap())).unwrap();
        assert!(stderr.contains("fake: waiting for stream"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_faulted_row_is_killed_for_no_recovery_and_never_for_a_stall() {
        let dir = scratch("unrecovered");
        let program = fake_launch(&dir, "echo 'fake: link down' >&2\nexec sleep 60");
        let silent = silent_observe_listener();
        let mut config = config(&dir, &program, silent.local_addr().unwrap().port());
        config.pipelines[0].tier = Tier::Faulted;
        let state = SharedState::default();
        let (unrecovered, requests) = tokio::sync::mpsc::unbounded_channel();
        let supervising = tokio::spawn(supervisor(&config, &state).run(requests));
        // past the stall limit without a frame
        tokio::time::sleep(Duration::from_secs(STALL_SECS * 2)).await;
        let pid = {
            let state = state.lock().unwrap();
            assert_eq!(
                state.restarts, 0,
                "a faulted row is never killed for a stall"
            );
            state.pid.expect("the run is still alive")
        };
        let fault = "long link outage";
        unrecovered
            .send(UnrecoveredRun {
                pid,
                fault: fault.into(),
            })
            .unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        supervising.abort();

        assert_eq!(state.lock().unwrap().restarts, 1);
        let open = reloaded_open_incidents(&dir);
        assert_eq!(open.len(), 1, "{open:?}");
        assert_eq!(
            open[0].cause,
            Cause::NoRecovery {
                fault: fault.into()
            }
        );
        let stderr = std::fs::read_to_string(dir.join(open[0].stderr.as_ref().unwrap())).unwrap();
        assert!(stderr.contains("fake: link down"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn failures_during_the_startup_grace_open_no_incident() {
        let dir = scratch("grace");
        let program = fake_launch(&dir, "echo 'pipeline error: not published yet' >&2\nexit 1");
        let grace_secs = OBSERVE_FOR.as_secs() + 1;
        let silent = silent_observe_listener();
        let port = silent.local_addr().unwrap().port();
        let config = config_with_grace(&dir, &program, port, grace_secs);
        let state = SharedState::default();
        supervise_for(&config, &state).await;

        assert!(state.lock().unwrap().restarts >= 2);
        assert!(reloaded_open_incidents(&dir).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_pipeline_error_names_the_element_error_before_it() {
        let cause = "rtsp://camera/stream: forbidden sequence 00 00 00 in NAL";
        let stderr = [
            format!("ERROR rtspsrc          {cause}"),
            "conn: 172.31.18.201:59558(me)->195.3.171.158:1935".to_string(),
            "ERROR runtime          pipeline error in RtspSrc0: Hardware(Other)".to_string(),
            "pipeline error: Hardware(Other)".to_string(),
        ];
        let mut seen = None;
        let shown: Vec<String> = stderr
            .iter()
            .map(|line| shown_error(line, &mut seen))
            .collect();
        assert_eq!(
            shown.last().unwrap(),
            &format!("pipeline error: Hardware(Other) (rtspsrc: {cause})")
        );
        assert_eq!(shown[1], stderr[1], "other lines show as they are");
    }

    #[tokio::test]
    async fn repeated_exits_stay_one_incident() {
        let dir = scratch("exit");
        let program = fake_launch(&dir, "echo 'pipeline error: boom' >&2\nexit 3");
        let silent = silent_observe_listener();
        let config = config(&dir, &program, silent.local_addr().unwrap().port());
        let state = SharedState::default();
        supervise_for(&config, &state).await;

        let state = state.lock().unwrap();
        let open = reloaded_open_incidents(&dir);
        assert_eq!(open.len(), 1);
        let incident = &open[0];
        assert_eq!(incident.cause, Cause::Exit { code: 3 });
        assert_eq!(incident.last_error.as_deref(), Some("pipeline error: boom"));
        assert!(state.restarts >= 2, "restarts {}", state.restarts);
        assert_eq!(incident.restarts, state.restarts);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    async fn serve_growing_telemetry(listener: tokio::net::TcpListener) {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                let mut packets = 0;
                loop {
                    packets += PACKETS_PER_TICK;
                    let telemetry = format!(
                        r#"{{"type":"telemetry","nodes":[{{"id":0,"name":"FfmpegVideoDec0","role":"transform","proc":{{"count":{packets},"p50_ns":2000000}}}},{{"id":1,"role":"sink","proc":null}}],"edges":[{{"from":0,"to":1,"packets":{packets}}}]}}"#
                    );
                    if socket.send(Message::text(telemetry)).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(TELEMETRY_INTERVAL).await;
                }
            });
        }
    }

    fn earlier_incident() -> Incident {
        Incident {
            id: "fake-1".into(),
            pipeline: "fake".into(),
            tier: Tier::SelfHosted,
            start: 1,
            end: None,
            cause: Cause::Exit { code: 1 },
            restarts: 1,
            last_error: None,
            stderr: None,
        }
    }

    // an incident the previous canary process left open on disk
    fn leave_open(dir: &Path, incident: Incident) {
        OpenIncidents::load(dir).unwrap().put(incident).unwrap();
    }

    #[tokio::test]
    async fn flowing_frames_close_an_incident_from_before_the_restart() {
        let dir = scratch("flow");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_growing_telemetry(listener));
        let program = fake_launch(&dir, "exec sleep 60");
        let config = config(&dir, &program, port);
        leave_open(&dir, earlier_incident());
        let state = SharedState::default();
        supervise_for(&config, &state).await;

        let state = state.lock().unwrap();
        assert_eq!(
            state.restarts, 0,
            "a run with frames flowing must not stall"
        );
        assert!(state.frames_total >= PACKETS_PER_TICK * 10);
        assert_eq!(state.decode_latency_ns, Some(2_000_000));
        assert!(reloaded_open_incidents(&dir).is_empty());
        let closed: Vec<Incident> = read_days(&dir, INCIDENTS_PREFIX, 0, unix_now()).unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(
            closed[0].id,
            earlier_incident().id,
            "the id survives the restart"
        );
        assert!(closed[0].end.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_stream_still_down_after_a_restart_extends_the_same_incident() {
        let dir = scratch("still-down");
        let program = fake_launch(&dir, "exit 1");
        let silent = silent_observe_listener();
        let config = config(&dir, &program, silent.local_addr().unwrap().port());
        leave_open(&dir, earlier_incident());
        let state = SharedState::default();
        supervise_for(&config, &state).await;

        let open = reloaded_open_incidents(&dir);
        assert_eq!(open.len(), 1, "no second incident: {open:?}");
        assert_eq!(open[0].id, earlier_incident().id);
        assert!(open[0].restarts > earlier_incident().restarts);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
