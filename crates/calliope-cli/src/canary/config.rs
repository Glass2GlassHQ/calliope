use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use super::recording;

const DEFAULT_SAMPLE_SECS: u64 = 60;
const DEFAULT_STALL_SECS: u64 = 30;
const DEFAULT_OBSERVE_BASE_PORT: u16 = 18700;
const DEFAULT_BACKOFF_MIN_SECS: u64 = 1;
const DEFAULT_BACKOFF_MAX_SECS: u64 = 300;
const DEFAULT_HEALTHY_RUN_SECS: u64 = 600;
const DEFAULT_STARTUP_GRACE_SECS: u64 = 120;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct CanaryConfig {
    pub log_dir: PathBuf,
    pub status_dir: PathBuf,
    // default: CALLIOPE_G2G_LAUNCH, then g2g-launch on PATH
    pub g2g_launch: Option<String>,
    #[serde(default = "default_sample_secs")]
    pub sample_secs: u64,
    // a run that decodes no frame for this long is killed
    #[serde(default = "default_stall_secs")]
    pub stall_secs: u64,
    // pipeline n serves its telemetry on this port + n
    #[serde(default = "default_observe_base_port")]
    pub observe_base_port: u16,
    #[serde(default = "default_backoff_min_secs")]
    pub backoff_min_secs: u64,
    #[serde(default = "default_backoff_max_secs")]
    pub backoff_max_secs: u64,
    // a run that lasted this long resets the restart backoff
    #[serde(default = "default_healthy_run_secs")]
    pub healthy_run_secs: u64,
    // failures this soon after the canary starts, or after an `ends` row ends, open no incident
    #[serde(default = "default_startup_grace_secs")]
    pub startup_grace_secs: u64,
    // relative to the working directory, a recording row writes its files under <recordings-dir>/<id>
    pub recordings_dir: Option<PathBuf>,
    #[serde(rename = "pipeline")]
    pub pipelines: Vec<PipelineConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PipelineConfig {
    pub id: String,
    pub tier: Tier,
    // the g2g-launch pipeline description
    pub launch: String,
    // a clean exit after decoding is the end the stream has by design and opens no incident
    #[serde(default)]
    pub ends: bool,
    // shown beside the launch line on the status page
    pub note: Option<String>,
    // the `name=` of the checksumsink ending the video branch
    pub video_sink: Option<String>,
    // the `name=` of the checksumsink ending the audio branch, for a/v drift
    pub audio_sink: Option<String>,
    // the video sink's digests of the looped clip, one checksumsink line per frame
    pub expect_hashes: Option<PathBuf>,
    // the launch line's splitmuxsink starts a new file this often, see recording::launch_line
    pub recording_segment_secs: Option<u64>,
}

// self-hosted streams separate a g2g failure from the internet failing
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    SelfHosted,
    Public,
    // self-hosted streams pushed through injected network faults
    Faulted,
}

impl Tier {
    pub const ALL: [Tier; 3] = [Tier::SelfHosted, Tier::Public, Tier::Faulted];

    pub fn label(self) -> &'static str {
        match self {
            Tier::SelfHosted => "self-hosted",
            Tier::Public => "public",
            Tier::Faulted => "faulted",
        }
    }
}

fn default_sample_secs() -> u64 {
    DEFAULT_SAMPLE_SECS
}
fn default_stall_secs() -> u64 {
    DEFAULT_STALL_SECS
}
fn default_observe_base_port() -> u16 {
    DEFAULT_OBSERVE_BASE_PORT
}
fn default_backoff_min_secs() -> u64 {
    DEFAULT_BACKOFF_MIN_SECS
}
fn default_backoff_max_secs() -> u64 {
    DEFAULT_BACKOFF_MAX_SECS
}
fn default_healthy_run_secs() -> u64 {
    DEFAULT_HEALTHY_RUN_SECS
}
fn default_startup_grace_secs() -> u64 {
    DEFAULT_STARTUP_GRACE_SECS
}

impl CanaryConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let config: CanaryConfig =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        ensure!(!self.pipelines.is_empty(), "no [[pipeline]] configured");
        ensure!(self.sample_secs > 0, "sample-secs must be positive");
        ensure!(self.stall_secs > 0, "stall-secs must be positive");
        ensure!(
            self.backoff_min_secs > 0 && self.backoff_min_secs <= self.backoff_max_secs,
            "need 0 < backoff-min-secs <= backoff-max-secs"
        );
        let mut seen = HashSet::new();
        for pipeline in &self.pipelines {
            // the id names incident and stderr files
            let file_safe = !pipeline.id.is_empty()
                && pipeline
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !file_safe {
                bail!("pipeline id '{}' must be [A-Za-z0-9_-]+", pipeline.id);
            }
            ensure!(
                seen.insert(&pipeline.id),
                "duplicate pipeline id '{}'",
                pipeline.id
            );
            let checks_without_video_sink = pipeline.video_sink.is_none()
                && (pipeline.expect_hashes.is_some() || pipeline.audio_sink.is_some());
            ensure!(
                !checks_without_video_sink,
                "pipeline '{}': expect-hashes and audio-sink need video-sink",
                pipeline.id
            );
            ensure!(
                pipeline.video_sink.is_none() || pipeline.video_sink != pipeline.audio_sink,
                "pipeline '{}': video-sink and audio-sink name the same sink",
                pipeline.id
            );
            let recovery_checked =
                pipeline.video_sink.is_some() && pipeline.expect_hashes.is_some();
            ensure!(
                pipeline.tier != Tier::Faulted || recovery_checked,
                "pipeline '{}': a faulted row needs video-sink and expect-hashes for its recovery check",
                pipeline.id
            );
            recording::validate(pipeline, self.recordings_dir.as_deref())
                .with_context(|| format!("pipeline '{}'", pipeline.id))?;
        }
        let last_port = usize::from(self.observe_base_port) + self.pipelines.len() - 1;
        ensure!(
            last_port <= usize::from(u16::MAX),
            "observe-base-port leaves no room for {} pipelines",
            self.pipelines.len()
        );
        Ok(())
    }

    pub fn observe_port(&self, index: usize) -> u16 {
        // validate() bounds base + index to u16
        self.observe_base_port + index as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<CanaryConfig> {
        let config: CanaryConfig = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    const TWO_PIPELINES: &str = r#"
        log-dir = "log"
        status-dir = "status"
        stall-secs = 20
        [[pipeline]]
        id = "baseline-rtsp"
        tier = "self-hosted"
        launch = "rtspsrc location=rtsp://127.0.0.1:8554/baseline ! decodebin ! fakesink"
        [[pipeline]]
        id = "mux-vod"
        tier = "public"
        launch = "hlssrc location=https://example.invalid/x.m3u8 ! decodebin ! fakesink"
        ends = true
    "#;

    #[test]
    fn parses_tiers_defaults_and_ports() {
        let config = parse(TWO_PIPELINES).unwrap();
        assert_eq!(config.stall_secs, 20);
        assert_eq!(config.sample_secs, DEFAULT_SAMPLE_SECS);
        assert_eq!(config.pipelines[0].tier, Tier::SelfHosted);
        assert!(config.pipelines[1].ends);
        assert_eq!(config.observe_port(1), DEFAULT_OBSERVE_BASE_PORT + 1);
    }

    #[test]
    fn parses_frame_checks_and_the_faulted_tier() {
        let checked = TWO_PIPELINES.replace("tier = \"public\"", "tier = \"faulted\"").replace(
            "ends = true",
            "ends = true\nvideo-sink = \"video\"\naudio-sink = \"audio\"\nexpect-hashes = \"/etc/canary/hashes/mux.txt\"",
        );
        let config = parse(&checked).unwrap();
        let pipeline = &config.pipelines[1];
        assert_eq!(pipeline.tier, Tier::Faulted);
        assert_eq!(pipeline.video_sink.as_deref(), Some("video"));
        assert_eq!(pipeline.audio_sink.as_deref(), Some("audio"));
        assert_eq!(
            pipeline.expect_hashes.as_deref(),
            Some(Path::new("/etc/canary/hashes/mux.txt"))
        );
        let no_video_sink = checked.replace("video-sink = \"video\"\n", "");
        assert!(parse(&no_video_sink).is_err());
        let no_hashes = checked.replace("expect-hashes = \"/etc/canary/hashes/mux.txt\"", "");
        assert!(
            parse(&no_hashes).is_err(),
            "a faulted row without a content check has no recovery check"
        );
    }

    #[test]
    fn rejects_duplicate_and_unsafe_ids() {
        let duplicate = TWO_PIPELINES.replace("mux-vod", "baseline-rtsp");
        assert!(parse(&duplicate).is_err());
        let unsafe_id = TWO_PIPELINES.replace("mux-vod", "../escape");
        assert!(parse(&unsafe_id).is_err());
    }
}
