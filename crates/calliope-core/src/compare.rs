//! Exact per-frame comparison against the reference engine. v1 asserts only
//! on bit-exact stages (spec-deterministic decode); perceptual tolerance
//! metrics are a later, advisory layer.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    pub matched: bool,
    pub reference_frames: usize,
    pub frames: usize,
    /// first differing frame index, when both sides have that frame
    pub first_divergence: Option<usize>,
}

pub fn compare(reference: &[String], candidate: &[String]) -> Comparison {
    let first_divergence = reference
        .iter()
        .zip(candidate)
        .position(|(r, c)| !r.eq_ignore_ascii_case(c));
    Comparison {
        matched: first_divergence.is_none() && reference.len() == candidate.len(),
        reference_frames: reference.len(),
        frames: candidate.len(),
        first_divergence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn detects_match_divergence_and_length_mismatch() {
        assert!(compare(&h(&["a", "b"]), &h(&["a", "b"])).matched);
        // case-insensitive hex
        assert!(compare(&h(&["AB"]), &h(&["ab"])).matched);

        let diverged = compare(&h(&["a", "b", "c"]), &h(&["a", "x", "c"]));
        assert!(!diverged.matched);
        assert_eq!(diverged.first_divergence, Some(1));

        let short = compare(&h(&["a", "b"]), &h(&["a"]));
        assert!(!short.matched);
        assert_eq!(short.first_divergence, None);
    }
}
