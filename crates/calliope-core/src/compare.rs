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

/// Divergence attribution across engines. With three or more engines that
/// decoded, the largest group sharing identical frame hashes is the majority;
/// an engine outside it is the likely culprit, even when that engine is the
/// reference. This turns a bare "diverged from reference" into "which engine is
/// wrong", and stops a reference quirk from masking a real bug elsewhere.
#[derive(Debug, Clone, Serialize)]
pub struct MajorityVote {
    /// engine ids in the largest identical-hash group
    pub majority: Vec<String>,
    /// engine ids that disagree with the majority (the suspects)
    pub outliers: Vec<String>,
    /// true when the majority is a strict majority (more than half of the
    /// engines); a tie or all-different set is inconclusive
    pub conclusive: bool,
}

impl MajorityVote {
    pub fn is_outlier(&self, engine: &str) -> bool {
        self.outliers.iter().any(|e| e == engine)
    }
}

/// Group engines by identical frame-hash vectors and pick the majority. Only
/// meaningful with three or more voters; fewer, or a tie for largest, is
/// `conclusive == false`. Engines are compared case-insensitively per frame.
pub fn majority_vote(engines: &[(String, Vec<String>)]) -> MajorityVote {
    // groups of engine indices that share an identical hash vector
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (i, (_, hashes)) in engines.iter().enumerate() {
        match groups
            .iter_mut()
            .find(|g| frames_equal(&engines[g[0]].1, hashes))
        {
            Some(g) => g.push(i),
            None => groups.push(vec![i]),
        }
    }
    let largest = groups.iter().map(Vec::len).max().unwrap_or(0);
    let tie = groups.iter().filter(|g| g.len() == largest).count() > 1;
    let conclusive = engines.len() >= 3 && !tie && largest * 2 > engines.len();
    let id = |i: usize| engines[i].0.clone();
    let (majority, outliers) = groups
        .iter()
        .partition::<Vec<_>, _>(|g| conclusive && g.len() == largest);
    MajorityVote {
        majority: majority.into_iter().flatten().map(|&i| id(i)).collect(),
        outliers: outliers.into_iter().flatten().map(|&i| id(i)).collect(),
        conclusive,
    }
}

fn frames_equal(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
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

    fn engine(id: &str, frames: &[&str]) -> (String, Vec<String>) {
        (id.to_string(), h(frames))
    }

    #[test]
    fn majority_vote_names_the_lone_dissenter() {
        // ffmpeg + gstreamer agree, g2g differs -> g2g is the outlier
        let vote = majority_vote(&[
            engine("ffmpeg", &["a", "b"]),
            engine("gstreamer", &["a", "b"]),
            engine("g2g", &["a", "x"]),
        ]);
        assert!(vote.conclusive);
        assert!(vote.is_outlier("g2g"));
        assert!(!vote.is_outlier("ffmpeg"));

        // the reference itself can be the outlier: the other two agree
        let vote = majority_vote(&[
            engine("ffmpeg", &["a", "z"]),
            engine("gstreamer", &["a", "b"]),
            engine("g2g", &["a", "b"]),
        ]);
        assert!(vote.conclusive);
        assert!(vote.is_outlier("ffmpeg"));
    }

    #[test]
    fn majority_vote_inconclusive_without_a_strict_majority() {
        // two engines that disagree: a tie, no majority
        let two = majority_vote(&[engine("ffmpeg", &["a"]), engine("g2g", &["b"])]);
        assert!(!two.conclusive);

        // three engines, all different: no group is a majority
        let all_diff = majority_vote(&[
            engine("ffmpeg", &["a"]),
            engine("gstreamer", &["b"]),
            engine("g2g", &["c"]),
        ]);
        assert!(!all_diff.conclusive);

        // unanimous: one group of all three, no outliers
        let same = majority_vote(&[
            engine("ffmpeg", &["a"]),
            engine("gstreamer", &["a"]),
            engine("g2g", &["a"]),
        ]);
        assert!(same.conclusive);
        assert!(same.outliers.is_empty());
    }
}
