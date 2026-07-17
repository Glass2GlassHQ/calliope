//! Subprocess execution: one engine invocation per run dir, with timeout
//! kill, stdout/stderr capture to files, crash/signal detection, and a peak
//! RSS watermark polled from /proc while the child runs (Linux only).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::engine::{Engine, OutputSpec};
use crate::scenario::Scenario;
use crate::{Error, Result, framehash};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunStatus {
    Ok,
    ExitFailure { code: i32 },
    Signaled { signal: i32 },
    Timeout,
    Error { message: String },
}

impl RunStatus {
    /// Robustness verdict: a corrupt input may be rejected (non-zero exit) but
    /// must not crash the process or hang. A `Signaled` (SIGSEGV / SIGABRT) or
    /// `Timeout` is a hardening bug; a harness `Error` means we could not judge.
    pub fn survived_corrupt_input(&self) -> bool {
        matches!(self, RunStatus::Ok | RunStatus::ExitFailure { .. })
    }
}

#[derive(Debug, Serialize)]
pub struct RunResult {
    pub engine: String,
    pub status: RunStatus,
    pub duration_ms: u128,
    pub peak_rss_kb: Option<u64>,
    #[serde(skip)]
    pub frame_hashes: Option<Vec<String>>,
    pub log_dir: PathBuf,
}

/// run one engine on one scenario; never panics on engine failure, every
/// outcome (crash, timeout, bad output) lands in `RunStatus`
pub async fn run_one(
    engine: &dyn Engine,
    scenario: &Scenario,
    input: &Path,
    workdir: &Path,
) -> RunResult {
    let run_dir = workdir.join(&scenario.id).join(engine.id());
    let fail = |message: String| RunResult {
        engine: engine.id().to_string(),
        status: RunStatus::Error { message },
        duration_ms: 0,
        peak_rss_kb: None,
        frame_hashes: None,
        log_dir: run_dir.clone(),
    };

    if let Err(e) = std::fs::create_dir_all(&run_dir) {
        return fail(e.to_string());
    }
    let invocation = match engine.plan(scenario, input, &run_dir) {
        Ok(i) => i,
        Err(e) => return fail(e.to_string()),
    };

    let stdout = match std::fs::File::create(run_dir.join("stdout.log")) {
        Ok(f) => f,
        Err(e) => return fail(e.to_string()),
    };
    let stderr = match std::fs::File::create(run_dir.join("stderr.log")) {
        Ok(f) => f,
        Err(e) => return fail(e.to_string()),
    };

    let started = Instant::now();
    let mut child = match tokio::process::Command::new(&invocation.program)
        .args(&invocation.args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return fail(format!("spawn {}: {e}", invocation.program)),
    };

    let peak_rss = Arc::new(AtomicU64::new(0));
    let watcher = child.id().map(|pid| {
        let peak = Arc::clone(&peak_rss);
        tokio::spawn(async move {
            loop {
                if let Some(kb) = read_vm_hwm_kb(pid) {
                    peak.fetch_max(kb, Ordering::Relaxed);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    });

    let timeout = Duration::from_secs(scenario.timeout_secs);
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => exit_to_status(status),
        Ok(Err(e)) => RunStatus::Error {
            message: e.to_string(),
        },
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            RunStatus::Timeout
        }
    };
    if let Some(w) = watcher {
        w.abort();
    }
    let duration_ms = started.elapsed().as_millis();
    let peak = peak_rss.load(Ordering::Relaxed);
    let peak_rss_kb = (peak > 0).then_some(peak);

    // a robustness scenario judges the exit status only; a corrupt stream is
    // not expected to produce comparable frames, so skip hash extraction
    let frame_hashes = if !scenario.is_robustness() && matches!(status, RunStatus::Ok) {
        match extract_hashes(&invocation.output, scenario) {
            Ok(h) => Some(h),
            Err(e) => {
                return RunResult {
                    engine: engine.id().to_string(),
                    status: RunStatus::Error {
                        message: e.to_string(),
                    },
                    duration_ms,
                    peak_rss_kb,
                    frame_hashes: None,
                    log_dir: run_dir,
                };
            }
        }
    } else {
        None
    };

    RunResult {
        engine: engine.id().to_string(),
        status,
        duration_ms,
        peak_rss_kb,
        frame_hashes,
        log_dir: run_dir,
    }
}

fn extract_hashes(output: &OutputSpec, scenario: &Scenario) -> Result<Vec<String>> {
    let hashes = match output {
        OutputSpec::FrameMd5File(path) => {
            framehash::parse_framemd5(&std::fs::read_to_string(path)?)
        }
        OutputSpec::RawVideoFile(path) => {
            let video = scenario
                .video
                .ok_or_else(|| Error::Parse("raw-dump engine needs [video] geometry".into()))?;
            framehash::hash_raw_dump(path, video.frame_size())?
        }
    };
    if hashes.is_empty() {
        return Err(Error::Parse("engine produced no frames".into()));
    }
    Ok(hashes)
}

fn exit_to_status(status: std::process::ExitStatus) -> RunStatus {
    if status.success() {
        return RunStatus::Ok;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return RunStatus::Signaled { signal };
        }
    }
    RunStatus::ExitFailure {
        code: status.code().unwrap_or(-1),
    }
}

/// VmHWM from /proc/<pid>/status; already a high-water mark, so the last
/// successful poll before exit is the peak
fn read_vm_hwm_kb(pid: u32) -> Option<u64> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{EngineInfo, Invocation};
    use crate::scenario::{Input, PixelFormat, Scenario, Video};

    struct FakeEngine {
        program: &'static str,
        args: Vec<String>,
    }

    impl Engine for FakeEngine {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn probe(&self) -> Result<EngineInfo> {
            Ok(EngineInfo {
                id: "fake".into(),
                version: "test".into(),
            })
        }
        fn plan(&self, _s: &Scenario, _input: &Path, workdir: &Path) -> Result<Invocation> {
            Ok(Invocation {
                program: self.program.into(),
                args: self.args.clone(),
                output: OutputSpec::RawVideoFile(workdir.join("out.yuv")),
            })
        }
    }

    fn scenario(timeout_secs: u64) -> Scenario {
        Scenario {
            id: "runner-test".into(),
            engines: vec!["fake".into()],
            reference: "fake".into(),
            timeout_secs,
            input: Input {
                corpus: None,
                path: Some("/dev/null".into()),
            },
            video: Some(Video {
                width: 2,
                height: 2,
                format: PixelFormat::I420,
            }),
            fault: None,
        }
    }

    fn workdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("calliope-runner-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[tokio::test]
    async fn captures_success_and_hashes_output() {
        let wd = workdir("ok");
        // 2x2 i420 frame = 6 bytes; emit two frames
        let engine = FakeEngine {
            program: "sh",
            args: vec![
                "-c".into(),
                format!(
                    "printf 'aaaaaabbbbbb' > {}/runner-test/fake/out.yuv",
                    wd.display()
                ),
            ],
        };
        let result = run_one(&engine, &scenario(30), Path::new("/dev/null"), &wd).await;
        assert!(
            matches!(result.status, RunStatus::Ok),
            "{:?}",
            result.status
        );
        assert_eq!(result.frame_hashes.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn reports_exit_failure_and_timeout() {
        let wd = workdir("fail");
        let engine = FakeEngine {
            program: "sh",
            args: vec!["-c".into(), "exit 3".into()],
        };
        let result = run_one(&engine, &scenario(30), Path::new("/dev/null"), &wd).await;
        assert!(matches!(result.status, RunStatus::ExitFailure { code: 3 }));

        let wd = workdir("timeout");
        let engine = FakeEngine {
            program: "sleep",
            args: vec!["10".into()],
        };
        let result = run_one(&engine, &scenario(1), Path::new("/dev/null"), &wd).await;
        assert!(matches!(result.status, RunStatus::Timeout));
    }
}
