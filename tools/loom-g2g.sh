#!/usr/bin/env bash
# Model-check g2g-core's hand-rolled lock-free SpscFrameRing under loom. loom
# runs the producer/consumer test under every thread interleaving of the no-CAS
# Acquire/Release protocol, exhaustively (Miri's single run cannot). The ring's
# atomics + UnsafeCell route through `crate::sync`, which swaps in loom's
# primitives under `--cfg loom`; the normal build is unchanged. Extra args pass
# through (e.g. a test-name filter).
#
# env: LOOM_MAX_PREEMPTIONS (interleaving depth bound, default 3),
# LOOM_LOG (set for per-branch logging).
set -euo pipefail

G2G_SRC="${G2G_SRC:-$HOME/src/glass2glass}"
[ -d "$G2G_SRC" ] || { echo "g2g source not at $G2G_SRC (set G2G_SRC)" >&2; exit 1; }

cd "$G2G_SRC"
LOOM_MAX_PREEMPTIONS="${LOOM_MAX_PREEMPTIONS:-3}" \
    RUSTFLAGS="--cfg loom" \
    cargo test -p g2g-core --features std --release --lib loom_tests "$@"
