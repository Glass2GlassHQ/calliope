//! Declarative scenario spec, loaded from TOML. Three modes:
//! - differential: decode the input and compare per-frame hashes against a
//!   reference engine (`[video]` geometry, or probed via ffprobe).
//! - robustness: corrupt the input via `[fault]` and assert every engine
//!   degrades gracefully instead of crashing or hanging.
//! - soak: repeat the run via `[soak]` and assert it never crashes or hangs.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::fault::Fault;
use crate::{Error, Result};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Scenario {
    pub id: String,
    /// engines to run; unknown ids fail loudly at run time
    pub engines: Vec<String>,
    /// engine whose frame hashes are the oracle; must be in `engines`
    pub reference: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    pub input: Input,
    /// decoded geometry for a differential scenario; omit to probe it (ffprobe)
    pub video: Option<Video>,
    /// present for a robustness scenario: corrupt the input before running
    pub fault: Option<Fault>,
    /// present for a soak scenario: repeat the run and watch for instability
    pub soak: Option<Soak>,
    /// golden scenario: assert each engine's whole decoded output matches the
    /// corpus vector's `decoded-md5` (the official conformance hash). No
    /// reference engine; requires a `corpus` input carrying that hash + format.
    #[serde(default)]
    pub golden: bool,
}

/// Repeat the run `iterations` times and assert it never crashes or hangs on
/// any iteration. Catches intermittent / order-dependent failures a single
/// pass misses. Each iteration is a fresh process, so this is a stability
/// probe, not a within-process memory-leak endurance test.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Soak {
    pub iterations: usize,
}

impl Scenario {
    /// robustness scenarios assert graceful degradation, not frame equality
    pub fn is_robustness(&self) -> bool {
        self.fault.is_some()
    }

    pub fn is_soak(&self) -> bool {
        self.soak.is_some()
    }

    pub fn is_golden(&self) -> bool {
        self.golden
    }

    /// only a plain differential scenario hashes and compares decoded frames
    /// per-frame against a reference engine
    pub fn judges_frames(&self) -> bool {
        self.fault.is_none() && self.soak.is_none() && !self.golden
    }
}

fn default_timeout() -> u64 {
    120
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Input {
    /// corpus vector id, resolved and fetched through the manifest
    pub corpus: Option<String>,
    /// local file, relative paths resolve against the scenario file's dir
    pub path: Option<PathBuf>,
}

/// Decoded geometry. Given in the scenario, or filled by [`crate::probe`]
/// (ffprobe) when a differential scenario omits `[video]`. Raw-dump engines
/// are hashed by chunking their output into frames of this size, so it must
/// match the format ffmpeg decodes natively (the engines convert to it as an
/// identity, not a lossy re-sample).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Video {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
}

/// 8-bit planar YUV, the formats we can chunk and cross-check. Others (10-bit,
/// packed, NV12) are rejected by the probe with a clear message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PixelFormat {
    I420,
    I422,
    I444,
}

impl PixelFormat {
    /// map an ffprobe `pix_fmt` string to a supported format
    pub fn from_pix_fmt(pix_fmt: &str) -> Option<Self> {
        match pix_fmt {
            "yuv420p" => Some(Self::I420),
            "yuv422p" => Some(Self::I422),
            "yuv444p" => Some(Self::I444),
            _ => None,
        }
    }

    /// the format name GStreamer's `video/x-raw,format=` uses
    pub fn gst_format(self) -> &'static str {
        match self {
            Self::I420 => "I420",
            Self::I422 => "Y42B",
            Self::I444 => "Y444",
        }
    }

    /// the format name g2g's `videoconvert format=` uses
    pub fn g2g_format(self) -> &'static str {
        match self {
            Self::I420 => "i420",
            Self::I422 => "i422",
            Self::I444 => "i444",
        }
    }

    /// the ffmpeg `pix_fmt` / `format=` filter name
    pub fn ffmpeg_pix_fmt(self) -> &'static str {
        match self {
            Self::I420 => "yuv420p",
            Self::I422 => "yuv422p",
            Self::I444 => "yuv444p",
        }
    }
}

impl Video {
    /// bytes per frame, matching ffmpeg's tightly packed framemd5 layout
    pub fn frame_size(&self) -> usize {
        let (w, h) = (self.width as usize, self.height as usize);
        let luma = w * h;
        // chroma plane dimensions per subsampling; two planes (U, V)
        let chroma = match self.format {
            PixelFormat::I420 => w.div_ceil(2) * h.div_ceil(2),
            PixelFormat::I422 => w.div_ceil(2) * h,
            PixelFormat::I444 => w * h,
        };
        luma + 2 * chroma
    }
}

impl Scenario {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut scenario: Scenario =
            toml::from_str(&text).map_err(|e| Error::Parse(format!("{}: {e}", path.display())))?;
        scenario.validate(path)?;
        // anchor relative input paths to the scenario file, not the cwd
        if let Some(p) = &scenario.input.path
            && p.is_relative()
        {
            let base = path.parent().unwrap_or(Path::new("."));
            scenario.input.path = Some(base.join(p));
        }
        Ok(scenario)
    }

    fn validate(&self, path: &Path) -> Result<()> {
        let at = || path.display();
        if !self.engines.contains(&self.reference) {
            return Err(Error::Parse(format!(
                "{}: reference engine '{}' is not in engines",
                at(),
                self.reference
            )));
        }
        if !matches!(
            (&self.input.corpus, &self.input.path),
            (Some(_), None) | (None, Some(_))
        ) {
            return Err(Error::Parse(format!(
                "{}: input needs exactly one of corpus / path",
                at()
            )));
        }
        let modes = self.fault.is_some() as u8 + self.soak.is_some() as u8 + self.golden as u8;
        if modes > 1 {
            return Err(Error::Parse(format!(
                "{}: fault / soak / golden are separate modes, use one",
                at()
            )));
        }
        // golden reads the expected hash from the corpus vector, so it needs one
        if self.golden && self.input.corpus.is_none() {
            return Err(Error::Parse(format!(
                "{}: a golden scenario needs a corpus input (its decoded-md5 is the oracle)",
                at()
            )));
        }
        // a differential scenario needs decoded geometry, but it may be probed
        // from the input at run time (ffprobe), so `[video]` is optional here.
        if let Some(fault) = &self.fault {
            fault
                .validate()
                .map_err(|e| Error::Parse(format!("{}: {e}", at())))?;
        }
        if let Some(soak) = &self.soak
            && soak.iterations < 2
        {
            return Err(Error::Parse(format!(
                "{}: soak iterations must be >= 2",
                at()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_size_i420() {
        let v = Video {
            width: 176,
            height: 144,
            format: PixelFormat::I420,
        };
        assert_eq!(v.frame_size(), 176 * 144 * 3 / 2);
        // odd dimensions round chroma planes up, like yuv420p
        let odd = Video {
            width: 177,
            height: 145,
            format: PixelFormat::I420,
        };
        assert_eq!(odd.frame_size(), 177 * 145 + 2 * (89 * 73));
    }

    #[test]
    fn frame_size_422_and_444() {
        let dims = (100, 60);
        let i422 = Video {
            width: dims.0,
            height: dims.1,
            format: PixelFormat::I422,
        };
        // 4:2:2 halves chroma width only
        assert_eq!(i422.frame_size(), 100 * 60 + 2 * (50 * 60));
        let i444 = Video {
            width: dims.0,
            height: dims.1,
            format: PixelFormat::I444,
        };
        assert_eq!(i444.frame_size(), 100 * 60 * 3);
    }

    #[test]
    fn pixel_format_from_pix_fmt() {
        assert_eq!(
            PixelFormat::from_pix_fmt("yuv420p"),
            Some(PixelFormat::I420)
        );
        assert_eq!(
            PixelFormat::from_pix_fmt("yuv422p"),
            Some(PixelFormat::I422)
        );
        assert_eq!(
            PixelFormat::from_pix_fmt("yuv444p"),
            Some(PixelFormat::I444)
        );
        assert_eq!(PixelFormat::from_pix_fmt("yuv420p10le"), None);
    }

    #[test]
    fn parses_and_validates() {
        let toml = r#"
            id = "smoke"
            engines = ["ffmpeg", "gstreamer"]
            reference = "ffmpeg"

            [input]
            path = "clip.h264"

            [video]
            width = 176
            height = 144
            format = "i420"
        "#;
        let s: Scenario = toml::from_str(toml).unwrap();
        s.validate(Path::new("test.toml")).unwrap();
        assert_eq!(s.timeout_secs, 120);
    }

    #[test]
    fn rejects_reference_not_in_engines() {
        let toml = r#"
            id = "bad"
            engines = ["gstreamer"]
            reference = "ffmpeg"
            [input]
            path = "clip.h264"
            [video]
            width = 16
            height = 16
            format = "i420"
        "#;
        let s: Scenario = toml::from_str(toml).unwrap();
        assert!(s.validate(Path::new("test.toml")).is_err());
    }

    #[test]
    fn robustness_scenario_needs_no_video() {
        let toml = r#"
            id = "fuzz"
            engines = ["ffmpeg", "g2g"]
            reference = "ffmpeg"

            [input]
            path = "clip.ts"

            [fault]
            mode = "bit-flip"
            count = 200
        "#;
        let s: Scenario = toml::from_str(toml).unwrap();
        s.validate(Path::new("test.toml")).unwrap();
        assert!(s.is_robustness());
    }

    #[test]
    fn differential_without_video_is_allowed_and_geometry_probed_at_runtime() {
        let toml = r#"
            id = "auto-geom"
            engines = ["ffmpeg", "gstreamer"]
            reference = "ffmpeg"
            [input]
            path = "clip.264"
        "#;
        let s: Scenario = toml::from_str(toml).unwrap();
        // valid at load; geometry comes from ffprobe when the run resolves it
        s.validate(Path::new("test.toml")).unwrap();
        assert!(s.judges_frames() && s.video.is_none());
    }
}
