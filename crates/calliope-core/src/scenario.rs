//! Declarative scenario spec, loaded from TOML. Two modes:
//! - differential: decode the input and compare per-frame hashes against a
//!   reference engine (needs `[video]` geometry).
//! - robustness: corrupt the input via `[fault]` and assert every engine
//!   degrades gracefully instead of crashing or hanging.

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
    /// decoded geometry, required for a differential scenario (no `[fault]`)
    pub video: Option<Video>,
    /// present for a robustness scenario: corrupt the input before running
    pub fault: Option<Fault>,
    /// present for a soak scenario: repeat the run and watch for instability
    pub soak: Option<Soak>,
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

    /// only a plain differential scenario hashes and compares decoded frames
    pub fn judges_frames(&self) -> bool {
        self.fault.is_none() && self.soak.is_none()
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

/// Expected decoded geometry. Explicit rather than probed in v1: raw-dump
/// engines are hashed by chunking the dump into frames of this size.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Video {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PixelFormat {
    I420,
}

impl Video {
    /// bytes per frame, matching ffmpeg's tightly packed framemd5 layout
    pub fn frame_size(&self) -> usize {
        let (w, h) = (self.width as usize, self.height as usize);
        let chroma = w.div_ceil(2) * h.div_ceil(2);
        match self.format {
            PixelFormat::I420 => w * h + 2 * chroma,
        }
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
        if self.fault.is_some() && self.soak.is_some() {
            return Err(Error::Parse(format!(
                "{}: [fault] and [soak] are separate modes, use one",
                at()
            )));
        }
        // a differential scenario compares decoded frames, so it needs geometry;
        // robustness / soak scenarios do not judge frames
        if self.judges_frames() && self.video.is_none() {
            return Err(Error::Parse(format!(
                "{}: a differential scenario needs [video] (or add [fault] / [soak])",
                at()
            )));
        }
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
    fn rejects_differential_scenario_without_video() {
        let toml = r#"
            id = "no-geom"
            engines = ["ffmpeg"]
            reference = "ffmpeg"
            [input]
            path = "clip.ts"
        "#;
        let s: Scenario = toml::from_str(toml).unwrap();
        assert!(s.validate(Path::new("test.toml")).is_err());
    }
}
