use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::engine::binary;

// fragment target both producers use for fragmented mp4 and hls fmp4 segments
pub const FRAGMENT_DURATION: Duration = Duration::from_secs(2);

const WEBRTC_SERVER_VARIABLE: &str = "CALLIOPE_WEBRTC_SERVER";
// mediamtx's default webrtc listener, it relays a WHIP publish to WHEP readers
const DEFAULT_WEBRTC_SERVER: &str = "http://127.0.0.1:8889";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Produce {
    pub container: Container,
    pub video: VideoCodec,
    // false remuxes the input's video, which must already be `video`
    #[serde(default = "default_encode_video")]
    pub encode_video: bool,
    pub audio: Option<ProducedAudio>,
    pub validators: Vec<Validator>,
}

fn default_encode_video() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Container {
    Mp4,
    // fragmented mp4, every track in one file
    Fmp4,
    // CMAF track file: one video track, fragments start at a sync sample
    Cmaf,
    Mkv,
    Ts,
    HlsTs,
    HlsFmp4,
    Dash,
    // live WebRTC publish to a WHIP endpoint, played back over WHEP
    Whip,
}

impl Container {
    // the file a validator opens in the engine's run dir, segments sit next to it
    pub fn artifact_name(self) -> Option<&'static str> {
        match self {
            Self::Mp4 | Self::Fmp4 | Self::Cmaf => Some("out.mp4"),
            Self::Mkv => Some("out.mkv"),
            Self::Ts => Some("out.ts"),
            Self::HlsTs | Self::HlsFmp4 => Some("index.m3u8"),
            Self::Dash => Some("manifest.mpd"),
            Self::Whip => None,
        }
    }

    pub fn is_live(self) -> bool {
        self == Self::Whip
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum VideoCodec {
    H264,
    H265,
    Av1,
}

impl VideoCodec {
    pub fn ffprobe_name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "hevc",
            Self::Av1 => "av1",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProducedAudio {
    // passed through from the input untouched
    Aac,
    // transcoded from the input's audio
    Opus,
}

impl ProducedAudio {
    pub fn ffprobe_name(self) -> &'static str {
        match self {
            Self::Aac => "aac",
            Self::Opus => "opus",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Validator {
    Ffprobe,
    Mp4box,
    Mkvalidator,
    Tsduck,
    DashIf,
    Mediastreamvalidator,
    Player,
}

impl Validator {
    // the name a scenario's `validators` list spells it with
    pub fn name(self) -> &'static str {
        match self {
            Self::Ffprobe => "ffprobe",
            Self::Mp4box => "mp4box",
            Self::Mkvalidator => "mkvalidator",
            Self::Tsduck => "tsduck",
            Self::DashIf => "dash-if",
            Self::Mediastreamvalidator => "mediastreamvalidator",
            Self::Player => "player",
        }
    }

    pub fn accepts(self, container: Container) -> bool {
        use Container::*;
        match self {
            Self::Ffprobe => !container.is_live(),
            Self::Mp4box => matches!(container, Mp4 | Fmp4 | Cmaf),
            Self::Mkvalidator => container == Mkv,
            Self::Tsduck => matches!(container, Ts | HlsTs),
            Self::DashIf => container == Dash,
            Self::Mediastreamvalidator => matches!(container, HlsTs | HlsFmp4),
            Self::Player => matches!(container, Mp4 | Fmp4 | Cmaf | HlsTs | HlsFmp4 | Dash | Whip),
        }
    }
}

// what every producer's output must carry, probed from the input
#[derive(Debug, Clone, Copy)]
pub struct Expectation {
    pub video: VideoCodec,
    pub audio: Option<ProducedAudio>,
    pub video_frames: u64,
    pub frame_rate: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    Pass,
    Fail,
    // the validator could not run here (tool absent, platform)
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
pub struct Validation {
    pub validator: Validator,
    pub verdict: Verdict,
    pub messages: Vec<String>,
}

impl Validation {
    pub fn pass(validator: Validator, messages: Vec<String>) -> Self {
        Self {
            validator,
            verdict: Verdict::Pass,
            messages,
        }
    }

    pub fn fail(validator: Validator, messages: Vec<String>) -> Self {
        Self {
            validator,
            verdict: Verdict::Fail,
            messages,
        }
    }

    pub fn skipped(validator: Validator, reason: String) -> Self {
        Self {
            validator,
            verdict: Verdict::Skipped,
            messages: vec![reason],
        }
    }

    // pass when nothing was found, else fail with the findings
    pub fn from_findings(validator: Validator, findings: Vec<String>) -> Self {
        if findings.is_empty() {
            Self::pass(validator, findings)
        } else {
            Self::fail(validator, findings)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Finding {
    Pass,
    Fail,
    // the control producer's output failed the same validator
    Advisory,
    Skipped,
}

pub fn finding(validation: &Validation, control: Option<&Validation>) -> Finding {
    match validation.verdict {
        Verdict::Pass => Finding::Pass,
        Verdict::Skipped => Finding::Skipped,
        Verdict::Fail if control.is_some_and(|c| c.verdict == Verdict::Fail) => Finding::Advisory,
        Verdict::Fail => Finding::Fail,
    }
}

fn webrtc_server() -> String {
    binary(WEBRTC_SERVER_VARIABLE, DEFAULT_WEBRTC_SERVER)
}

// one stream path per engine, so producers running side by side never collide
pub fn whip_endpoint(engine: &str) -> String {
    format!("{}/{engine}/whip", webrtc_server())
}

pub fn whep_endpoint(engine: &str) -> String {
    format!("{}/{engine}/whep", webrtc_server())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validation(verdict: Verdict) -> Validation {
        Validation {
            validator: Validator::Ffprobe,
            verdict,
            messages: Vec::new(),
        }
    }

    #[test]
    fn a_failure_the_control_shares_is_advisory() {
        let fail = validation(Verdict::Fail);
        let pass = validation(Verdict::Pass);
        assert_eq!(finding(&fail, Some(&fail)), Finding::Advisory);
        assert_eq!(finding(&fail, Some(&pass)), Finding::Fail);
        assert_eq!(finding(&fail, None), Finding::Fail);
        assert_eq!(finding(&pass, Some(&fail)), Finding::Pass);
    }

    #[test]
    fn validators_only_accept_containers_they_understand() {
        assert!(Validator::Mkvalidator.accepts(Container::Mkv));
        assert!(!Validator::Mkvalidator.accepts(Container::Mp4));
        assert!(Validator::Tsduck.accepts(Container::HlsTs));
        assert!(!Validator::Ffprobe.accepts(Container::Whip));
        assert!(Validator::Player.accepts(Container::Whip));
    }
}
