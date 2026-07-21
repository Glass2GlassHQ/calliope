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
    /// soak / determinism only: how many iterations ran (all of them if it
    /// never crashed or diverged)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iterations_completed: Option<usize>,
    /// MD5 of the whole output artifact. Golden compares it to the vector's
    /// conformance `decoded-md5`; determinism compares it across runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_md5: Option<String>,
    /// Byte length of the whole output artifact. Lets golden tell a geometry
    /// divergence (an engine decoding to a different size) from a same-size hash
    /// mismatch, used to exclude gstreamer's alignment-cropped output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_len: Option<u64>,
    /// determinism only: whether every run's output was byte-identical
    #[serde(skip_serializing_if = "Option::is_none")]
    pub determinism_matched: Option<bool>,
    /// roundtrip only: PSNR (dB) of the re-encoded, re-decoded output vs the
    /// reference decode of the original. Filled by the CLI after ffmpeg validates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub psnr: Option<f64>,
    /// outcome-diff only: did this engine actually decode frames from the
    /// corrupt input (`Some(true)`) or exit clean without producing any
    /// (`Some(false)`)? `None` outside outcome-diff. A crash / hang / non-zero
    /// exit is carried by `status`, not here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decoded: Option<bool>,
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
    match engine.plan(scenario, input, &run_dir) {
        Ok(invocation) => exec(engine.id(), scenario, invocation, run_dir).await,
        Err(e) => fail_result(engine.id(), run_dir, e.to_string()),
    }
}

fn fail_result(engine: &str, log_dir: PathBuf, message: String) -> RunResult {
    RunResult {
        engine: engine.to_string(),
        status: RunStatus::Error { message },
        duration_ms: 0,
        peak_rss_kb: None,
        frame_hashes: None,
        log_dir,
        iterations_completed: None,
        output_md5: None,
        output_len: None,
        determinism_matched: None,
        psnr: None,
        decoded: None,
    }
}

/// Execute one prepared invocation in `run_dir`: spawn, enforce the timeout,
/// capture logs and peak RSS, and derive the scenario's output (per-frame
/// hashes for differential, whole-artifact MD5 for golden / determinism).
async fn exec(
    engine_id: &str,
    scenario: &Scenario,
    invocation: crate::engine::Invocation,
    run_dir: PathBuf,
) -> RunResult {
    let fail = |message: String| fail_result(engine_id, run_dir.clone(), message);

    if let Err(e) = std::fs::create_dir_all(&run_dir) {
        return fail(e.to_string());
    }

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

    // only a differential scenario compares frames; robustness / soak judge the
    // exit status only, so skip hash extraction for them
    let frame_hashes = if scenario.judges_frames() && matches!(status, RunStatus::Ok) {
        match extract_hashes(&invocation.output, scenario) {
            Ok(h) => Some(h),
            Err(e) => return fail(e.to_string()),
        }
    } else {
        None
    };

    // golden compares the whole decoded output to the conformance hash;
    // determinism compares the whole output artifact across runs; the
    // resolution-change oracle checks the whole decoded byte count against the
    // expected per-frame size sum. Each needs the whole-artifact size / hash.
    let (output_md5, output_len) =
        if (scenario.is_golden() || scenario.is_determinism() || scenario.is_resolution_change())
            && matches!(status, RunStatus::Ok)
        {
            match &invocation.output {
                OutputSpec::RawVideoFile(path)
                | OutputSpec::RawAudioFile(path)
                | OutputSpec::FrameMd5File(path) => (
                    whole_file_md5(path).ok(),
                    std::fs::metadata(path).map(|m| m.len()).ok(),
                ),
                // a roundtrip's encoded stream is validated by ffmpeg (PSNR), not
                // hashed here
                OutputSpec::EncodedFile(_) => (None, None),
            }
        } else {
            (None, None)
        };

    // outcome-diff: classify decoded (produced frames) vs rejected (clean exit,
    // no frames) on the corrupt input. A pixel compare is noise here, so the
    // cross-engine signal is this structural outcome.
    let decoded = if scenario.is_outcome_diff() && matches!(status, RunStatus::Ok) {
        Some(produced_output(&invocation.output))
    } else {
        None
    };

    RunResult {
        engine: engine_id.to_string(),
        status,
        duration_ms,
        peak_rss_kb,
        frame_hashes,
        log_dir: run_dir,
        iterations_completed: None,
        output_md5,
        output_len,
        determinism_matched: None,
        psnr: None,
        decoded,
    }
}

/// Did the engine actually decode any frames? For ffmpeg's framemd5 the file
/// carries a comment header even on a zero-frame decode, so count real frame
/// lines; a raw dump has no header, so any bytes mean at least a partial frame.
fn produced_output(output: &OutputSpec) -> bool {
    match output {
        OutputSpec::FrameMd5File(path) => std::fs::read_to_string(path)
            .map(|t| !framehash::parse_framemd5(&t).is_empty())
            .unwrap_or(false),
        OutputSpec::RawVideoFile(path) | OutputSpec::RawAudioFile(path) => std::fs::metadata(path)
            .map(|m| m.len() > 0)
            .unwrap_or(false),
        OutputSpec::EncodedFile(_) => false,
    }
}

/// streaming MD5 of a whole file (the golden conformance hash)
fn whole_file_md5(path: &Path) -> Result<String> {
    use md5::{Digest, Md5};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Md5::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Repeat [`run_one`] `iterations` times (a soak scenario), stopping at the
/// first crash or hang. Returns a single `RunResult` whose status is the first
/// non-surviving outcome (else `Ok`), `peak_rss_kb` the max seen, and
/// `iterations_completed` how many ran. Each iteration is a fresh process.
pub async fn run_soak(
    engine: &dyn Engine,
    scenario: &Scenario,
    input: &Path,
    workdir: &Path,
) -> RunResult {
    let iterations = scenario.soak.map(|s| s.iterations).unwrap_or(1);
    let mut peak_rss_kb: Option<u64> = None;
    let mut total_ms = 0u128;
    let mut last = None;
    let mut completed = 0;
    for _ in 0..iterations {
        let r = run_one(engine, scenario, input, workdir).await;
        completed += 1;
        total_ms += r.duration_ms;
        peak_rss_kb = peak_rss_kb.max(r.peak_rss_kb);
        let survived = r.status.survived_corrupt_input();
        last = Some(r);
        if !survived {
            break;
        }
    }
    let mut result = last.expect("soak runs at least once");
    result.duration_ms = total_ms;
    result.peak_rss_kb = peak_rss_kb;
    result.iterations_completed = Some(completed);
    result
}

/// Run an engine repeatedly (a determinism scenario) plus, if requested and the
/// engine has one, a threaded variant, and assert every run's output artifact
/// is byte-identical. A crash / hang stops early and is surfaced as-is. The
/// returned `RunResult` carries `determinism_matched` and the agreed hash in
/// `output_md5`.
pub async fn run_determinism(
    engine: &dyn Engine,
    scenario: &Scenario,
    input: &Path,
    workdir: &Path,
) -> RunResult {
    let det = scenario
        .determinism
        .expect("determinism scenario has [determinism]");
    let mut hashes: Vec<String> = Vec::new();
    let mut total_ms = 0u128;
    let mut peak_rss_kb: Option<u64> = None;
    let mut last = None;
    let mut completed = 0;
    for _ in 0..det.runs {
        let r = run_one(engine, scenario, input, workdir).await;
        completed += 1;
        total_ms += r.duration_ms;
        peak_rss_kb = peak_rss_kb.max(r.peak_rss_kb);
        let ok = matches!(r.status, RunStatus::Ok);
        if let Some(h) = &r.output_md5 {
            hashes.push(h.clone());
        }
        last = Some(r);
        // a crash / hang / error is itself a determinism failure worth surfacing
        if !ok {
            break;
        }
    }

    // threaded variant, when the engine defines one: its own run dir so its
    // output does not clobber the base runs' artifact
    if det.threads
        && last
            .as_ref()
            .is_some_and(|r| matches!(r.status, RunStatus::Ok))
    {
        let run_dir = workdir
            .join(&scenario.id)
            .join(format!("{}-threaded", engine.id()));
        match engine.threaded_plan(scenario, input, &run_dir) {
            Ok(Some(invocation)) => {
                let r = exec(engine.id(), scenario, invocation, run_dir).await;
                total_ms += r.duration_ms;
                peak_rss_kb = peak_rss_kb.max(r.peak_rss_kb);
                match &r.status {
                    // its output must match the base runs' hash
                    RunStatus::Ok => {
                        if let Some(h) = &r.output_md5 {
                            hashes.push(h.clone());
                        }
                    }
                    // a crash / hang under threading is a real hardening bug
                    RunStatus::Signaled { .. } | RunStatus::Timeout => last = Some(r),
                    // a plain non-zero exit / harness error means the threaded
                    // variant could not run (e.g. g2g built without the
                    // multi-thread feature); skip it rather than false-fail
                    RunStatus::ExitFailure { .. } | RunStatus::Error { .. } => {}
                }
            }
            Ok(None) => {}
            Err(_) => {}
        }
    }

    let mut result = last.expect("determinism runs at least once");
    result.duration_ms = total_ms;
    result.peak_rss_kb = peak_rss_kb;
    result.iterations_completed = Some(completed);
    let matched = !hashes.is_empty()
        && matches!(result.status, RunStatus::Ok)
        && hashes.windows(2).all(|w| w[0].eq_ignore_ascii_case(&w[1]));
    result.determinism_matched = Some(matched);
    result.output_md5 = hashes.into_iter().next();
    result
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
        // audio compares the whole decoded PCM stream as a single hash (frame
        // boundaries differ across decoders); a divergence means the streams
        // are not byte-identical.
        OutputSpec::RawAudioFile(path) => {
            if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) == 0 {
                return Err(Error::Parse("engine produced no audio".into()));
            }
            vec![whole_file_md5(path)?]
        }
        // a roundtrip encoded stream is never frame-hashed (only golden/diff are)
        OutputSpec::EncodedFile(_) => Vec::new(),
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

// The fixtures drive real subprocesses via Unix shell builtins (sh / sleep /
// true); the runner logic they exercise is OS-agnostic, so running them on
// Linux + macOS covers it. Windows CI skips this module.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::engine::{EngineInfo, Invocation};
    use crate::scenario::{Input, PixelFormat, Scenario, Video};

    struct FakeEngine {
        program: &'static str,
        args: Vec<String>,
        /// determinism threaded variant; None means the engine has none
        threaded: Option<(&'static str, Vec<String>)>,
    }

    fn fake(program: &'static str, args: Vec<String>) -> FakeEngine {
        FakeEngine {
            program,
            args,
            threaded: None,
        }
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
        fn threaded_plan(
            &self,
            _s: &Scenario,
            _input: &Path,
            workdir: &Path,
        ) -> Result<Option<Invocation>> {
            Ok(self.threaded.as_ref().map(|(program, args)| Invocation {
                program: (*program).into(),
                args: args.clone(),
                output: OutputSpec::RawVideoFile(workdir.join("out.yuv")),
            }))
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
            audio: None,
            fault: None,
            soak: None,
            determinism: None,
            golden: false,
            roundtrip: None,
            encode: None,
            resolution_change: false,
            outcome_diff: false,
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
        let engine = fake(
            "sh",
            vec![
                "-c".into(),
                format!(
                    "printf 'aaaaaabbbbbb' > {}/runner-test/fake/out.yuv",
                    wd.display()
                ),
            ],
        );
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
        let engine = fake("sh", vec!["-c".into(), "exit 3".into()]);
        let result = run_one(&engine, &scenario(30), Path::new("/dev/null"), &wd).await;
        assert!(matches!(result.status, RunStatus::ExitFailure { code: 3 }));

        let wd = workdir("timeout");
        let engine = fake("sleep", vec!["10".into()]);
        let result = run_one(&engine, &scenario(1), Path::new("/dev/null"), &wd).await;
        assert!(matches!(result.status, RunStatus::Timeout));
    }

    fn soak_scenario(iterations: usize, timeout_secs: u64) -> Scenario {
        let mut s = scenario(timeout_secs);
        s.soak = Some(crate::scenario::Soak { iterations });
        s
    }

    #[tokio::test]
    async fn soak_runs_all_iterations_when_stable() {
        let wd = workdir("soak-ok");
        let engine = fake("true", vec![]);
        let result = run_soak(&engine, &soak_scenario(5, 30), Path::new("/dev/null"), &wd).await;
        assert!(result.status.survived_corrupt_input());
        assert_eq!(result.iterations_completed, Some(5));
    }

    #[tokio::test]
    async fn soak_stops_at_first_hang() {
        let wd = workdir("soak-hang");
        let engine = fake("sleep", vec!["10".into()]);
        let result = run_soak(&engine, &soak_scenario(5, 1), Path::new("/dev/null"), &wd).await;
        assert!(matches!(result.status, RunStatus::Timeout));
        // stopped on the very first iteration's hang, not all five
        assert_eq!(result.iterations_completed, Some(1));
    }

    fn determinism_scenario(runs: usize, threads: bool) -> Scenario {
        let mut s = scenario(30);
        s.determinism = Some(crate::scenario::Determinism { runs, threads });
        s
    }

    // write `count` distinct random bytes to the fake engine's output; each run
    // differs, so the determinism verdict must be a mismatch
    fn write_random(wd: &Path, count: usize) -> Vec<String> {
        vec![
            "-c".into(),
            format!(
                "head -c {count} /dev/urandom > {}/runner-test/fake/out.yuv",
                wd.display()
            ),
        ]
    }

    #[tokio::test]
    async fn determinism_matches_identical_output_and_flags_variation() {
        // fixed output every run -> byte-identical -> matched
        let wd = workdir("det-ok");
        let args = vec![
            "-c".into(),
            format!(
                "printf 'aaaaaabbbbbb' > {}/runner-test/fake/out.yuv",
                wd.display()
            ),
        ];
        let engine = fake("sh", args);
        let result = run_determinism(
            &engine,
            &determinism_scenario(3, false),
            Path::new("/dev/null"),
            &wd,
        )
        .await;
        assert_eq!(result.determinism_matched, Some(true));
        assert_eq!(result.iterations_completed, Some(3));

        // random output each run -> divergent hashes -> not matched
        let wd = workdir("det-bad");
        let engine = fake("sh", write_random(&wd, 12));
        let result = run_determinism(
            &engine,
            &determinism_scenario(3, false),
            Path::new("/dev/null"),
            &wd,
        )
        .await;
        assert_eq!(result.determinism_matched, Some(false));
    }

    #[tokio::test]
    async fn determinism_skips_unavailable_threaded_variant() {
        // base runs are stable; the threaded variant exits non-zero (as a build
        // lacking the feature would), so it is skipped, not treated as a failure
        let wd = workdir("det-threaded-skip");
        let mut engine = fake(
            "sh",
            vec![
                "-c".into(),
                format!(
                    "printf 'aaaaaabbbbbb' > {}/runner-test/fake/out.yuv",
                    wd.display()
                ),
            ],
        );
        engine.threaded = Some(("sh", vec!["-c".into(), "exit 1".into()]));
        let result = run_determinism(
            &engine,
            &determinism_scenario(2, true),
            Path::new("/dev/null"),
            &wd,
        )
        .await;
        assert_eq!(result.determinism_matched, Some(true));
    }
}
