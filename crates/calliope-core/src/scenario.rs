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
    #[serde(default)]
    pub input: Input,
    /// decoded geometry for a differential scenario; omit to probe it (ffprobe)
    pub video: Option<Video>,
    /// present for a robustness scenario: corrupt the input before running
    pub fault: Option<Fault>,
    /// corrupt-input differential: with `[fault]`, also cross-compare each
    /// engine's decode *outcome* (decoded vs rejected) against the reference,
    /// not just crash / hang. A pixel compare is meaningless on corrupt input
    /// (error concealment is implementation-defined), so the signal is
    /// structural: the high-value split is an engine decoding a stream the
    /// reference refused (the too-lenient-parser class). Divergences are
    /// advisory; only a crash / hang fails the run.
    #[serde(default)]
    pub outcome_diff: bool,
    /// present for a soak scenario: repeat the run and watch for instability
    pub soak: Option<Soak>,
    /// present for a determinism scenario: run each engine repeatedly (and,
    /// where the engine has one, a threaded variant) and assert every run's
    /// output is byte-identical. No reference engine; a self-comparison, so it
    /// isolates nondeterminism / threading bugs without reference-quirk noise.
    pub determinism: Option<Determinism>,
    /// golden scenario: assert each engine's whole decoded output matches the
    /// corpus vector's `decoded-md5` (the official conformance hash). No
    /// reference engine; requires a `corpus` input carrying that hash + format.
    #[serde(default)]
    pub golden: bool,
    /// encode round-trip scenario: the engine transcodes the input (decode ->
    /// re-encode with a named encoder); ffmpeg then decodes that bitstream and
    /// PSNR-compares it to the reference decode of the original. Fails if the
    /// encoder crashes, produces an undecodable stream, or drops below `psnr-min`.
    pub roundtrip: Option<Roundtrip>,
    /// encode-differential scenario: ffmpeg encodes a synthetic lavfi source,
    /// then the engines differential-decode the result (bit-exact). Exercises
    /// decoders on bitstreams the conformance corpus never produced. See
    /// [`Encode`]. Judged as a plain differential run.
    pub encode: Option<Encode>,
    /// resolution-change scenario: decode a stream whose frame geometry changes
    /// mid-playback and require each engine to survive (no crash / hang) and emit
    /// the expected total decoded bytes (the per-frame size sequence from
    /// ffprobe). Targets the engine's own caps / buffer renegotiation, not the
    /// codec core. No pixel oracle: ffmpeg's CLI normalizes output geometry on a
    /// resolution change, so it can't reference the pixels bit-exactly.
    #[serde(default)]
    pub resolution_change: bool,
}

/// Encode-differential config: ffmpeg encodes a lavfi source into an elementary
/// stream that then feeds the normal differential decode compare. ffmpeg goes
/// forward (encode), the other engines go reverse (decode), and the per-frame
/// compare is bit-exact. The `args` select profiles / features the Fluster
/// vectors lack, so a decode divergence here is a real decoder bug against a
/// hard oracle (unlike the PSNR round-trip, which only smoke-tests an encoder).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Encode {
    /// ffmpeg lavfi source spec (e.g. `testsrc2=size=352x288:rate=30:duration=2`);
    /// its size must match `[video]`.
    pub source: String,
    /// ffmpeg encoder that generates the stream (e.g. `libx264`).
    pub encoder: String,
    /// extra ffmpeg args selecting profile / features (e.g. `-profile:v high`).
    #[serde(default)]
    pub args: Vec<String>,
    /// the elementary stream's extension so ffmpeg / decodebin type it (e.g. `h264`).
    #[serde(default = "default_roundtrip_ext")]
    pub output_ext: String,
}

/// Encode round-trip config: transcode the input through `encoder`, then require
/// ffmpeg to decode the result at >= `psnr_min` dB versus the reference decode.
/// Exercises the encoders (undecodable output / crashes / gross corruption),
/// which the decode-only modes never touch.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Roundtrip {
    /// The engine's encoder element (e.g. `x264enc`), plugged after `decodebin`.
    pub encoder: String,
    /// Minimum acceptable PSNR (dB) of the round-tripped decode vs the reference.
    #[serde(default = "default_psnr_min")]
    pub psnr_min: f64,
    /// The encoded elementary stream's extension so ffmpeg types it (e.g. `h264`).
    #[serde(default = "default_roundtrip_ext")]
    pub output_ext: String,
}

fn default_psnr_min() -> f64 {
    30.0
}

fn default_roundtrip_ext() -> String {
    "h264".into()
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

/// Repeat each engine `runs` times and require byte-identical output every
/// time. With `threads`, also run the engine's threaded variant (if it has
/// one) and require it to match too, catching threading-order bugs.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Determinism {
    #[serde(default = "default_determinism_runs")]
    pub runs: usize,
    #[serde(default = "default_true")]
    pub threads: bool,
}

fn default_determinism_runs() -> usize {
    3
}

fn default_true() -> bool {
    true
}

impl Scenario {
    /// robustness scenarios assert graceful degradation, not frame equality
    pub fn is_robustness(&self) -> bool {
        self.fault.is_some()
    }

    /// corrupt-input differential: a robustness scenario that also cross-compares
    /// decode outcomes across engines
    pub fn is_outcome_diff(&self) -> bool {
        self.outcome_diff
    }

    pub fn is_soak(&self) -> bool {
        self.soak.is_some()
    }

    pub fn is_golden(&self) -> bool {
        self.golden
    }

    pub fn is_determinism(&self) -> bool {
        self.determinism.is_some()
    }

    pub fn is_roundtrip(&self) -> bool {
        self.roundtrip.is_some()
    }

    pub fn is_encode(&self) -> bool {
        self.encode.is_some()
    }

    pub fn is_resolution_change(&self) -> bool {
        self.resolution_change
    }

    /// A plain differential scenario hashes and compares decoded frames per-frame
    /// against a reference engine. An encode scenario is also differential: it
    /// just generates its input with ffmpeg first, so it judges frames too.
    pub fn judges_frames(&self) -> bool {
        self.fault.is_none()
            && self.soak.is_none()
            && !self.golden
            && self.determinism.is_none()
            && self.roundtrip.is_none()
            && !self.resolution_change
    }
}

fn default_timeout() -> u64 {
    120
}

#[derive(Debug, Clone, Default, Deserialize)]
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

/// Decoded pixel layouts we can chunk into frames and cross-check bit-exactly.
/// The planar YUV family (I420/I422/I444 at 8/10/12-bit) plus semi-planar NV12
/// are all identity conversions from a decoder's native output, so a raw dump
/// stays byte-identical to ffmpeg's. Packed RGB / YUYV are matrix- or
/// order-dependent (not bit-exact across engines) and stay unsupported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PixelFormat {
    I420,
    I422,
    I444,
    Nv12,
    #[serde(rename = "i420p10")]
    I420p10,
    #[serde(rename = "i420p12")]
    I420p12,
    #[serde(rename = "i422p10")]
    I422p10,
    #[serde(rename = "i422p12")]
    I422p12,
    #[serde(rename = "i444p10")]
    I444p10,
    #[serde(rename = "i444p12")]
    I444p12,
}

impl PixelFormat {
    /// map an ffprobe `pix_fmt` string to a supported format
    pub fn from_pix_fmt(pix_fmt: &str) -> Option<Self> {
        match pix_fmt {
            "yuv420p" => Some(Self::I420),
            "yuv422p" => Some(Self::I422),
            "yuv444p" => Some(Self::I444),
            "nv12" => Some(Self::Nv12),
            "yuv420p10le" => Some(Self::I420p10),
            "yuv420p12le" => Some(Self::I420p12),
            "yuv422p10le" => Some(Self::I422p10),
            "yuv422p12le" => Some(Self::I422p12),
            "yuv444p10le" => Some(Self::I444p10),
            "yuv444p12le" => Some(Self::I444p12),
            _ => None,
        }
    }

    /// the format name GStreamer's `video/x-raw,format=` uses
    pub fn gst_format(self) -> &'static str {
        match self {
            Self::I420 => "I420",
            Self::I422 => "Y42B",
            Self::I444 => "Y444",
            Self::Nv12 => "NV12",
            Self::I420p10 => "I420_10LE",
            Self::I420p12 => "I420_12LE",
            Self::I422p10 => "I422_10LE",
            Self::I422p12 => "I422_12LE",
            Self::I444p10 => "Y444_10LE",
            Self::I444p12 => "Y444_12LE",
        }
    }

    /// the ffmpeg `pix_fmt` / `format=` filter name
    pub fn ffmpeg_pix_fmt(self) -> &'static str {
        match self {
            Self::I420 => "yuv420p",
            Self::I422 => "yuv422p",
            Self::I444 => "yuv444p",
            Self::Nv12 => "nv12",
            Self::I420p10 => "yuv420p10le",
            Self::I420p12 => "yuv420p12le",
            Self::I422p10 => "yuv422p10le",
            Self::I422p12 => "yuv422p12le",
            Self::I444p10 => "yuv444p10le",
            Self::I444p12 => "yuv444p12le",
        }
    }

    /// bytes per sample: 2 for the 10-/12-bit formats (each sample a LE u16), else 1
    fn bytes_per_sample(self) -> usize {
        match self {
            Self::I420p10
            | Self::I420p12
            | Self::I422p10
            | Self::I422p12
            | Self::I444p10
            | Self::I444p12 => 2,
            _ => 1,
        }
    }

    /// true for the fully-planar I420/I422/I444 family (three separate Y/U/V
    /// planes), which the decoders emit directly; false for semi-planar NV12,
    /// which a planar decode reaches only through a videoconvert.
    pub fn is_planar_yuv(self) -> bool {
        !matches!(self, Self::Nv12)
    }

    /// chroma right-shift from luma dims as (horizontal, vertical): 4:2:0 =
    /// (1, 1), 4:2:2 = (1, 0), 4:4:4 = (0, 0). NV12 is 4:2:0 subsampled (its
    /// two chroma samples are interleaved, but the sample count matches I420).
    fn chroma_shift(self) -> (u32, u32) {
        match self {
            Self::I420 | Self::I420p10 | Self::I420p12 | Self::Nv12 => (1, 1),
            Self::I422 | Self::I422p10 | Self::I422p12 => (1, 0),
            Self::I444 | Self::I444p10 | Self::I444p12 => (0, 0),
        }
    }
}

impl Video {
    /// bytes per frame, matching ffmpeg's tightly packed framemd5 layout
    pub fn frame_size(&self) -> usize {
        let (w, h) = (self.width as usize, self.height as usize);
        let (sx, sy) = self.format.chroma_shift();
        let luma = w * h;
        // two chroma planes (U, V), each subsampled per the format; NV12
        // interleaves them but carries the same sample count
        let chroma = w.div_ceil(1 << sx) * h.div_ceil(1 << sy);
        (luma + 2 * chroma) * self.format.bytes_per_sample()
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
        // an encode scenario generates its input from a lavfi source, so it has
        // no corpus / path; every other scenario needs exactly one.
        if self.encode.is_none()
            && !matches!(
                (&self.input.corpus, &self.input.path),
                (Some(_), None) | (None, Some(_))
            )
        {
            return Err(Error::Parse(format!(
                "{}: input needs exactly one of corpus / path",
                at()
            )));
        }
        let modes = self.fault.is_some() as u8
            + self.soak.is_some() as u8
            + self.golden as u8
            + self.determinism.is_some() as u8
            + self.roundtrip.is_some() as u8
            + self.encode.is_some() as u8
            + self.resolution_change as u8;
        if modes > 1 {
            return Err(Error::Parse(format!(
                "{}: fault / soak / golden / determinism / roundtrip / encode / resolution-change are separate modes, use one",
                at()
            )));
        }
        if let Some(enc) = &self.encode {
            if self.video.is_none() {
                return Err(Error::Parse(format!(
                    "{}: an encode scenario needs [video] geometry (it decodes to it)",
                    at()
                )));
            }
            if self.input.corpus.is_some() || self.input.path.is_some() {
                return Err(Error::Parse(format!(
                    "{}: an encode scenario generates its input from a lavfi source; omit [input]",
                    at()
                )));
            }
            if enc.source.trim().is_empty() || enc.encoder.trim().is_empty() {
                return Err(Error::Parse(format!(
                    "{}: an encode scenario needs a non-empty source and encoder",
                    at()
                )));
            }
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
        if self.outcome_diff {
            if self.fault.is_none() {
                return Err(Error::Parse(format!(
                    "{}: outcome-diff needs a [fault] to corrupt the input",
                    at()
                )));
            }
            if self.engines.len() < 2 {
                return Err(Error::Parse(format!(
                    "{}: outcome-diff needs >= 2 engines to cross-compare outcomes",
                    at()
                )));
            }
        }
        if let Some(soak) = &self.soak
            && soak.iterations < 2
        {
            return Err(Error::Parse(format!(
                "{}: soak iterations must be >= 2",
                at()
            )));
        }
        if let Some(det) = &self.determinism
            && det.runs < 2
        {
            return Err(Error::Parse(format!(
                "{}: determinism runs must be >= 2",
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
        assert_eq!(PixelFormat::from_pix_fmt("nv12"), Some(PixelFormat::Nv12));
        assert_eq!(
            PixelFormat::from_pix_fmt("yuv420p10le"),
            Some(PixelFormat::I420p10)
        );
        assert_eq!(
            PixelFormat::from_pix_fmt("yuv444p12le"),
            Some(PixelFormat::I444p12)
        );
        // packed RGB stays unsupported (not bit-exact across engines)
        assert_eq!(PixelFormat::from_pix_fmt("rgb24"), None);
    }

    #[test]
    fn frame_size_nv12_and_high_bit_depth() {
        // NV12 carries the same byte count as I420 (interleaved chroma, 8-bit)
        let nv12 = Video {
            width: 176,
            height: 144,
            format: PixelFormat::Nv12,
        };
        assert_eq!(nv12.frame_size(), 176 * 144 * 3 / 2);
        // 10-bit 4:2:0 doubles every sample to a 2-byte word
        let p10 = Video {
            width: 176,
            height: 144,
            format: PixelFormat::I420p10,
        };
        assert_eq!(p10.frame_size(), 176 * 144 * 3 / 2 * 2);
        // 12-bit 4:4:4: three full-res planes, 2 bytes each
        let p12 = Video {
            width: 100,
            height: 60,
            format: PixelFormat::I444p12,
        };
        assert_eq!(p12.frame_size(), 100 * 60 * 3 * 2);
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
    fn encode_scenario_parses_is_differential_and_needs_video_but_no_input() {
        let toml = r#"
            id = "enc"
            engines = ["ffmpeg", "g2g"]
            reference = "ffmpeg"

            [encode]
            source = "testsrc2=size=352x288:rate=30:duration=2"
            encoder = "libx264"
            args = ["-profile:v", "high"]

            [video]
            width = 352
            height = 288
            format = "i420"
        "#;
        let s: Scenario = toml::from_str(toml).unwrap();
        s.validate(Path::new("test.toml")).unwrap();
        assert!(s.is_encode());
        // an encode run is judged as a plain differential (bit-exact frames)
        assert!(s.judges_frames());
        let enc = s.encode.unwrap();
        assert_eq!(enc.encoder, "libx264");
        assert_eq!(enc.output_ext, "h264");
    }

    #[test]
    fn encode_scenario_rejects_input_and_requires_video() {
        // [input] is generated, so declaring one is an error
        let with_input = r#"
            id = "enc"
            engines = ["ffmpeg", "g2g"]
            reference = "ffmpeg"
            [encode]
            source = "testsrc2=size=16x16:rate=1:duration=1"
            encoder = "libx264"
            [input]
            path = "clip.h264"
            [video]
            width = 16
            height = 16
            format = "i420"
        "#;
        let s: Scenario = toml::from_str(with_input).unwrap();
        assert!(s.validate(Path::new("test.toml")).is_err());

        // no [video] geometry -> error (the decode target is unknown)
        let no_video = r#"
            id = "enc"
            engines = ["ffmpeg", "g2g"]
            reference = "ffmpeg"
            [encode]
            source = "testsrc2=size=16x16:rate=1:duration=1"
            encoder = "libx264"
        "#;
        let s: Scenario = toml::from_str(no_video).unwrap();
        assert!(s.validate(Path::new("test.toml")).is_err());
    }

    #[test]
    fn resolution_change_parses_and_is_its_own_mode() {
        let toml = r#"
            id = "res"
            engines = ["g2g"]
            reference = "g2g"
            resolution-change = true
            [input]
            path = "change.h264"
        "#;
        let s: Scenario = toml::from_str(toml).unwrap();
        s.validate(Path::new("test.toml")).unwrap();
        assert!(s.is_resolution_change());
        // judged by survival + byte total, not per-frame comparison
        assert!(!s.judges_frames());
        // needs no [video]: geometry varies and is probed from the stream
        assert!(s.video.is_none());
    }

    #[test]
    fn resolution_change_excludes_other_modes() {
        let toml = r#"
            id = "res"
            engines = ["g2g"]
            reference = "g2g"
            resolution-change = true
            golden = true
            [input]
            corpus = "x"
        "#;
        let s: Scenario = toml::from_str(toml).unwrap();
        assert!(s.validate(Path::new("test.toml")).is_err());
    }

    #[test]
    fn encode_and_fault_are_mutually_exclusive() {
        let toml = r#"
            id = "enc"
            engines = ["ffmpeg", "g2g"]
            reference = "ffmpeg"
            [encode]
            source = "testsrc2=size=16x16:rate=1:duration=1"
            encoder = "libx264"
            [fault]
            mode = "bit-flip"
            count = 10
            [video]
            width = 16
            height = 16
            format = "i420"
        "#;
        let s: Scenario = toml::from_str(toml).unwrap();
        assert!(s.validate(Path::new("test.toml")).is_err());
    }

    #[test]
    fn outcome_diff_needs_fault_and_two_engines() {
        let ok = r#"
            id = "od"
            engines = ["ffmpeg", "g2g"]
            reference = "ffmpeg"
            outcome-diff = true
            [input]
            path = "clip.h264"
            [fault]
            mode = "nal-payload"
            count = 300
        "#;
        let s: Scenario = toml::from_str(ok).unwrap();
        s.validate(Path::new("test.toml")).unwrap();
        assert!(s.is_outcome_diff() && s.is_robustness());
        // still a robustness run: crash / hang judged, no pixel compare
        assert!(!s.judges_frames());

        // outcome-diff without a [fault] has nothing to corrupt
        let no_fault = r#"
            id = "od"
            engines = ["ffmpeg", "g2g"]
            reference = "ffmpeg"
            outcome-diff = true
            [input]
            path = "clip.h264"
        "#;
        let s: Scenario = toml::from_str(no_fault).unwrap();
        assert!(s.validate(Path::new("test.toml")).is_err());

        // one engine cannot cross-compare an outcome
        let one_engine = r#"
            id = "od"
            engines = ["g2g"]
            reference = "g2g"
            outcome-diff = true
            [input]
            path = "clip.h264"
            [fault]
            mode = "bit-flip"
            count = 100
        "#;
        let s: Scenario = toml::from_str(one_engine).unwrap();
        assert!(s.validate(Path::new("test.toml")).is_err());
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
