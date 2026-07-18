//! Deterministic input corruption for robustness scenarios. A corrupted
//! stream is not expected to decode bit-exactly (or at all); the oracle is
//! that an engine degrades gracefully (clean exit or error) rather than
//! crashing or hanging. This directly exercises parser / demuxer hardening
//! against attacker-controlled input.
//!
//! Corruption is seeded so a failure reproduces exactly from the scenario.

use std::path::Path;

use serde::Deserialize;

use crate::Result;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Fault {
    pub mode: FaultKind,
    #[serde(default = "default_seed")]
    pub seed: u64,
    /// bit-flip / byte-drop: how many operations to apply
    #[serde(default)]
    pub count: usize,
    /// truncate: percent of the file to keep from the front (1..=99)
    #[serde(default = "default_keep_percent")]
    pub keep_percent: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FaultKind {
    /// keep only the first `keep_percent` of the bytes (mid-stream cutoff)
    Truncate,
    /// flip `count` individual bits at seeded offsets
    BitFlip,
    /// delete `count` bytes at seeded offsets (shifts everything after)
    ByteDrop,
    /// flip `count` bits, but only inside Annex-B NAL payloads, leaving every
    /// start code and NAL-header byte intact. The stream still parses and the
    /// units still route to slice decode, so the corruption reaches the
    /// reconstruction path instead of dying at the demuxer / framer, where a
    /// blind bit-flip usually lands.
    NalPayload,
}

fn default_seed() -> u64 {
    1
}

fn default_keep_percent() -> u8 {
    50
}

impl Fault {
    pub fn validate(&self) -> core::result::Result<(), String> {
        match self.mode {
            FaultKind::BitFlip | FaultKind::ByteDrop | FaultKind::NalPayload if self.count == 0 => {
                Err(format!("{:?} fault needs count > 0", self.mode))
            }
            FaultKind::Truncate if !(1..=99).contains(&self.keep_percent) => {
                Err("truncate keep-percent must be 1..=99".into())
            }
            _ => Ok(()),
        }
    }

    /// Apply the corruption to `input`, returning the mangled bytes. An empty
    /// input is returned unchanged (nothing to corrupt).
    pub fn corrupt(&self, input: &[u8]) -> Vec<u8> {
        if input.is_empty() {
            return Vec::new();
        }
        let mut rng = self.seed;
        match self.mode {
            FaultKind::Truncate => {
                let keep = (input.len() * self.keep_percent as usize / 100).max(1);
                input[..keep].to_vec()
            }
            FaultKind::BitFlip => {
                let mut out = input.to_vec();
                for _ in 0..self.count {
                    let offset = (splitmix64(&mut rng) as usize) % out.len();
                    let bit = (splitmix64(&mut rng) % 8) as u8;
                    out[offset] ^= 1 << bit;
                }
                out
            }
            FaultKind::NalPayload => {
                let mut out = input.to_vec();
                let payload = nal_payload_offsets(input);
                // no Annex-B start codes: nothing safe to target, leave it be
                if payload.is_empty() {
                    return out;
                }
                for _ in 0..self.count {
                    let offset = payload[(splitmix64(&mut rng) as usize) % payload.len()];
                    let bit = (splitmix64(&mut rng) % 8) as u8;
                    out[offset] ^= 1 << bit;
                }
                out
            }
            FaultKind::ByteDrop => {
                // collect distinct drop offsets, then keep the survivors in order
                let drop_count = self.count.min(out_len_floor(input.len()));
                let mut drop = vec![false; input.len()];
                let mut dropped = 0;
                while dropped < drop_count {
                    let offset = (splitmix64(&mut rng) as usize) % input.len();
                    if !drop[offset] {
                        drop[offset] = true;
                        dropped += 1;
                    }
                }
                input
                    .iter()
                    .zip(drop)
                    .filter_map(|(b, dropped)| (!dropped).then_some(*b))
                    .collect()
            }
        }
    }

    /// Corrupt `src` into `dst`, returning the corrupted byte count.
    pub fn corrupt_file(&self, src: &Path, dst: &Path) -> Result<usize> {
        let input = std::fs::read(src)?;
        let out = self.corrupt(&input);
        std::fs::write(dst, &out)?;
        Ok(out.len())
    }
}

/// leave at least one byte so a fully-dropped file cannot look like a clean EOF
fn out_len_floor(len: usize) -> usize {
    len.saturating_sub(1)
}

/// Byte offsets that lie inside an Annex-B NAL payload: everything except each
/// three-byte start code (`00 00 01`) and the one NAL-header byte after it.
/// Protecting the header byte keeps the NAL type, so a corrupted unit still
/// routes to the right decode path. Empty if the stream has no start codes.
fn nal_payload_offsets(data: &[u8]) -> Vec<usize> {
    // positions of every `00 00 01` start-code prefix
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut offsets = Vec::new();
    for (n, &start) in starts.iter().enumerate() {
        // skip the 3-byte code and the 1-byte NAL header
        let payload_begin = start + 4;
        // payload runs up to the next start code (or end of stream); stop at the
        // next prefix so its leading zeros stay intact
        let payload_end = starts.get(n + 1).copied().unwrap_or(data.len());
        offsets.extend(payload_begin..payload_end);
    }
    offsets
}

/// splitmix64: a tiny deterministic PRNG, so a scenario's corruption is
/// reproducible without pulling a rand dependency.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fault(mode: FaultKind, count: usize, keep_percent: u8) -> Fault {
        Fault {
            mode,
            seed: 42,
            count,
            keep_percent,
        }
    }

    #[test]
    fn truncate_keeps_front_fraction() {
        let input = vec![0xABu8; 1000];
        let out = fault(FaultKind::Truncate, 0, 25).corrupt(&input);
        assert_eq!(out.len(), 250);
        assert!(out.iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn bit_flip_is_deterministic_and_bounded() {
        let input = vec![0u8; 500];
        let f = fault(FaultKind::BitFlip, 50, 0);
        let a = f.corrupt(&input);
        let b = f.corrupt(&input);
        assert_eq!(a, b, "same seed reproduces the exact corruption");
        assert_eq!(a.len(), input.len(), "bit-flip preserves length");
        let flipped = a.iter().filter(|&&x| x != 0).count();
        assert!(
            flipped > 0 && flipped <= 50,
            "at most `count` bytes changed: {flipped}"
        );

        // a different seed diverges
        let mut f2 = f.clone();
        f2.seed = 43;
        assert_ne!(f2.corrupt(&input), a);
    }

    #[test]
    fn byte_drop_shortens_and_keeps_one_byte() {
        let input: Vec<u8> = (0..200).map(|i| i as u8).collect();
        let out = fault(FaultKind::ByteDrop, 50, 0).corrupt(&input);
        assert_eq!(out.len(), 150);

        // dropping more than the file still leaves a byte
        let all = fault(FaultKind::ByteDrop, 10_000, 0).corrupt(&input);
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn validate_rejects_degenerate_config() {
        assert!(fault(FaultKind::BitFlip, 0, 0).validate().is_err());
        assert!(fault(FaultKind::Truncate, 0, 0).validate().is_err());
        assert!(fault(FaultKind::Truncate, 0, 50).validate().is_ok());
        assert!(fault(FaultKind::ByteDrop, 5, 0).validate().is_ok());
        assert!(fault(FaultKind::NalPayload, 0, 0).validate().is_err());
        assert!(fault(FaultKind::NalPayload, 10, 0).validate().is_ok());
    }

    #[test]
    fn nal_payload_offsets_exclude_start_codes_and_headers() {
        // two Annex-B NALs: 00 00 01 <header> <payload...>
        let data = [
            0, 0, 1, 0x65, 0xAA, 0xBB, 0xCC, // NAL 1: payload at 4,5,6
            0, 0, 1, 0x41, 0xDD, 0xEE, 0xFF, // NAL 2: payload at 11,12,13
        ];
        assert_eq!(nal_payload_offsets(&data), vec![4, 5, 6, 11, 12, 13]);
        // a stream with no start codes offers nothing safe to target
        assert!(nal_payload_offsets(&[1, 2, 3, 4]).is_empty());
    }

    #[test]
    fn nal_payload_flips_only_inside_payloads() {
        let data = vec![
            0, 0, 1, 0x65, 0xAA, 0xBB, 0xCC, 0, 0, 1, 0x41, 0xDD, 0xEE, 0xFF,
        ];
        let protected = [0usize, 1, 2, 3, 7, 8, 9, 10];
        let out = fault(FaultKind::NalPayload, 200, 0).corrupt(&data);
        assert_eq!(out.len(), data.len(), "bit-flips preserve length");
        for i in protected {
            assert_eq!(out[i], data[i], "start code / header byte {i} untouched");
        }
        assert!(
            (4..=6).chain(11..=13).any(|i| out[i] != data[i]),
            "at least one payload byte was flipped"
        );
        // same seed reproduces exactly
        assert_eq!(out, fault(FaultKind::NalPayload, 200, 0).corrupt(&data));
    }
}
