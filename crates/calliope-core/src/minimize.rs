//! Delta-debugging minimizer for a failing input. When an engine crashes or
//! hangs on a (usually corrupted) stream, shrink it to a minimal byte sequence
//! that still triggers the failure, so the reproducer is small enough to read.
//!
//! The reduction is the classic ddmin chunk-removal: remove a run of bytes and
//! keep the shorter input whenever the failure still reproduces, at decreasing
//! granularity. It only ever adopts a candidate the predicate confirms, so it
//! needs no monotonicity assumption about where the failure lives.

/// Shrink `initial` to a minimal subsequence for which `still_fails` holds.
/// The caller must have confirmed `still_fails(initial)` before calling.
/// `still_fails` should be deterministic (a flaky predicate yields a
/// non-minimal, still-valid reproducer, never a false one).
pub fn minimize(initial: &[u8], still_fails: &mut dyn FnMut(&[u8]) -> bool) -> Vec<u8> {
    let mut current = initial.to_vec();
    if current.len() <= 1 {
        return current;
    }
    let mut chunk = current.len() / 2;
    loop {
        let mut i = 0;
        while i < current.len() {
            let end = (i + chunk).min(current.len());
            let mut candidate = Vec::with_capacity(current.len() - (end - i));
            candidate.extend_from_slice(&current[..i]);
            candidate.extend_from_slice(&current[end..]);
            if !candidate.is_empty() && still_fails(&candidate) {
                current = candidate; // removed this run; re-test at the same offset
            } else {
                i += chunk;
            }
        }
        if chunk == 1 {
            break;
        }
        chunk = (chunk / 2).max(1);
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduces_to_the_essential_marker() {
        // failure = the contiguous marker [1,2,3] is present
        let initial = vec![0, 0, 1, 2, 3, 0, 0, 0, 0];
        let mut pred = |b: &[u8]| b.windows(3).any(|w| w == [1, 2, 3]);
        let min = minimize(&initial, &mut pred);
        assert_eq!(min, vec![1, 2, 3]);
    }

    #[test]
    fn reduces_to_minimal_length_property() {
        // failure = at least 4 bytes remain (any of them)
        let initial = vec![7u8; 64];
        let mut pred = |b: &[u8]| b.len() >= 4;
        let min = minimize(&initial, &mut pred);
        assert_eq!(min.len(), 4);
    }

    #[test]
    fn keeps_a_single_byte_when_that_is_the_trigger() {
        let initial = vec![9u8; 32];
        // failure = a 0x09 byte is present; one suffices
        let mut pred = |b: &[u8]| b.contains(&9);
        let min = minimize(&initial, &mut pred);
        assert_eq!(min, vec![9]);
    }

    #[test]
    fn never_returns_a_non_failing_result() {
        // a predicate needing two specific distant bytes; result must still fail
        let initial: Vec<u8> = (0..50).collect();
        let mut pred = |b: &[u8]| b.contains(&5) && b.contains(&40);
        let min = minimize(&initial, &mut pred);
        assert!(pred(&min));
        assert!(min.len() < initial.len());
    }
}
