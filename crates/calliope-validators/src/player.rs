use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

use calliope_core::engine::{OutputSpec, binary};
use calliope_core::produce::{Container, Expectation, Validation, Validator};
use serde::Deserialize;

use crate::run_command;

const DEFAULT_HARNESS: &str = "tools/playback/play.mjs";
// play.mjs exit status when it could not start a browser
const HARNESS_UNAVAILABLE_EXIT: i32 = 3;
const LIVE_WATCH_SECONDS: u64 = 6;
// webrtc drops frames while ice and dtls settle, so a live reader sees a share of them
const LIVE_MINIMUM_FRAME_FRACTION: f64 = 0.5;
// headroom past the clip's own duration for browser start and segment fetches
const PLAYBACK_STARTUP_SECONDS: u64 = 30;

// realtime playback stalls when browsers compete for the cpu, a live read cannot wait
static FILE_PLAYBACK: Mutex<()> = Mutex::new(());

#[derive(Debug, Deserialize)]
struct PlayerReport {
    frames: u64,
    error: Option<String>,
}

// the browser path that plays each container, webcodecs demuxes mp4 with mp4box.js
fn mode(container: Container) -> Option<&'static str> {
    match container {
        Container::HlsTs | Container::HlsFmp4 => Some("hls"),
        Container::Dash => Some("dash"),
        Container::Whip => Some("whep"),
        Container::Mp4 | Container::Fmp4 | Container::Cmaf => Some("webcodecs"),
        Container::Mkv | Container::Ts => None,
    }
}

// headless chromium plays (or WebCodecs-decodes) the output and counts the frames it rendered
pub fn validate(container: Container, output: &OutputSpec, expected: &Expectation) -> Validation {
    let Some(mode) = mode(container) else {
        return Validation::skipped(
            Validator::Player,
            format!("no browser path plays {container:?}"),
        );
    };
    let target = match output {
        OutputSpec::Produced(path) => path.display().to_string(),
        OutputSpec::Published { whep } => whep.clone(),
        _ => {
            return Validation::skipped(
                Validator::Player,
                "the engine produced nothing to play".into(),
            );
        }
    };
    let clip_seconds = (expected.video_frames as f64 / expected.frame_rate).ceil() as u64;
    let harness = binary("CALLIOPE_PLAYBACK", DEFAULT_HARNESS);
    let mut command = Command::new(binary("CALLIOPE_NODE", "node"));
    command
        .arg(Path::new(&harness))
        .args([mode, &target])
        .args(["--seconds", &LIVE_WATCH_SECONDS.to_string()])
        .args([
            "--timeout",
            &(clip_seconds + PLAYBACK_STARTUP_SECONDS).to_string(),
        ]);
    let one_at_a_time = (!container.is_live()).then(|| FILE_PLAYBACK.lock());
    let output = run_command(Validator::Player, &mut command);
    drop(one_at_a_time);
    let output = match output {
        Ok(output) => output,
        Err(skipped) => return skipped,
    };
    if output.status.code() == Some(HARNESS_UNAVAILABLE_EXIT) {
        return Validation::skipped(Validator::Player, output.stdout.trim().to_string());
    }
    let Ok(report) = serde_json::from_str::<PlayerReport>(output.stdout.trim()) else {
        return Validation::skipped(
            Validator::Player,
            format!(
                "no player report (pnpm install in tools/playback?): {}",
                output.stderr.trim()
            ),
        );
    };
    let minimum = if container.is_live() {
        (expected.frame_rate * LIVE_WATCH_SECONDS as f64 * LIVE_MINIMUM_FRAME_FRACTION) as u64
    } else {
        expected.video_frames
    };
    judge(&report, container.is_live(), minimum)
}

// a file plays every input frame, a live publish at least `minimum` in the watch window
fn judge(report: &PlayerReport, live: bool, minimum: u64) -> Validation {
    let mut findings: Vec<String> = report.error.iter().cloned().collect();
    let enough = if live {
        report.frames >= minimum
    } else {
        report.frames == minimum
    };
    if !enough {
        let wanted = if live { "at least" } else { "all" };
        findings.push(format!(
            "rendered {} frames, wanted {wanted} {minimum}",
            report.frames
        ));
    }
    let summary = format!("{} frames rendered", report.frames);
    if findings.is_empty() {
        Validation::pass(Validator::Player, vec![summary])
    } else {
        Validation::fail(Validator::Player, findings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use calliope_core::produce::Verdict;

    const CLIP_FRAMES: u64 = 300;

    fn report(json: &str) -> PlayerReport {
        serde_json::from_str(json.trim()).unwrap()
    }

    #[test]
    fn every_frame_decoded_passes() {
        let r = report(include_str!("../testdata/player-webcodecs-ok.json"));
        assert_eq!(judge(&r, false, CLIP_FRAMES).verdict, Verdict::Pass);
        assert_eq!(judge(&r, false, CLIP_FRAMES + 1).verdict, Verdict::Fail);
    }

    #[test]
    fn a_player_error_fails() {
        let r = report(include_str!("../testdata/player-hls-missing.json"));
        let v = judge(&r, false, CLIP_FRAMES);
        assert_eq!(v.verdict, Verdict::Fail);
        assert!(v.messages[0].contains("manifestLoadError"));
    }

    #[test]
    fn live_needs_only_the_minimum() {
        let r = report(include_str!("../testdata/player-webcodecs-ok.json"));
        assert_eq!(judge(&r, true, CLIP_FRAMES / 2).verdict, Verdict::Pass);
    }
}
