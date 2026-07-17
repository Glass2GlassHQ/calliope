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
            FaultKind::BitFlip | FaultKind::ByteDrop if self.count == 0 => {
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
    }
}
