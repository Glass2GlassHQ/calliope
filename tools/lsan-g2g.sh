#!/usr/bin/env bash
# Leak pass: run the conformance corpus on the ASan g2g-launch with leak
# detection ON. The fuzz / conformance ASan runs set detect_leaks=0 (a decoder
# that exits mid-stream on a fault leaves expected allocations); a clean whole-
# stream conformance decode should free everything, so any report here is a real
# leak in g2g's own code or the libav it drives. Build the ASan binary first
# with tools/build-g2g-asan.sh.
set -euo pipefail

ASAN_BIN="${CALLIOPE_G2G_LAUNCH:-$HOME/.local/bin/g2g-launch-asan}"
[ -x "$ASAN_BIN" ] || { echo "asan g2g-launch not at $ASAN_BIN (run build-g2g-asan.sh)" >&2; exit 1; }
LIMIT="${LSAN_LIMIT:-100}"

cd "$(dirname "$0")/.."
CALLIOPE_G2G_LAUNCH="$ASAN_BIN" \
ASAN_OPTIONS="detect_leaks=1:abort_on_error=1" \
LSAN_OPTIONS="${LSAN_OPTIONS:-}" \
    cargo run --release -p calliope-cli -- \
    conformance --corpus corpus/vectors.toml --engines g2g --limit "$LIMIT"
