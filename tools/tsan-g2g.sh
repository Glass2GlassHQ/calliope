#!/usr/bin/env bash
# Run g2g-core's threading under ThreadSanitizer: real OS threads, so TSan sees
# actual runtime interleavings of the SPSC ring / pools / runtime handoffs. This
# complements the other two concurrency checks, which never run true parallel
# code: Miri interprets one interleaving, loom model-checks the SPSC ring
# exhaustively but abstractly. TSan is scoped to g2g-core (its `multi-thread`
# tests spawn the producer / consumer threads); it can't instrument the C libav,
# so the whole-pipeline race surface stays with loom + Miri + the deterministic
# differential runs. Extra args pass through (e.g. a test-name filter).
#
# needs nightly + rust-src (for -Zbuild-std, so std is TSan-instrumented too).
set -euo pipefail

G2G_SRC="${G2G_SRC:-$HOME/src/glass2glass}"
TARGET=x86_64-unknown-linux-gnu

[ -d "$G2G_SRC" ] || { echo "g2g source not at $G2G_SRC (set G2G_SRC)" >&2; exit 1; }
rustup component add rust-src --toolchain nightly >/dev/null 2>&1 || true

cd "$G2G_SRC"
# --tests: run the unit / integration test binaries, not doctests. Doctests
# compile via rustdoc (no RUSTFLAGS), so under -Zbuild-std they mix a sanitized
# `spin` / std with an unsanitized crate and fail the ABI-mismatch check; they
# carry no concurrency coverage anyway.
RUSTFLAGS="-Zsanitizer=thread" \
TSAN_OPTIONS="${TSAN_OPTIONS:-halt_on_error=1}" \
    cargo +nightly test -Z build-std --target "$TARGET" \
    -p g2g-core --features std,multi-thread --tests "$@"
