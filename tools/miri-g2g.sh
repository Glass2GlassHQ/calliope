#!/usr/bin/env bash
# Run g2g's pure-Rust unsafe under Miri (undefined-behavior, data-race, and leak
# detection). Miri interprets MIR, so it catches aliasing / stacked-borrows UB
# and data races that ASan and fuzzing miss. It cannot execute C FFI
# (ffmpeg / dav1d) or real IO, so this scopes to g2g-core, the no_std pools,
# SPSC ring, and runtime, which hold the soundness-critical unsafe. Extra args
# pass through (e.g. a test-name filter).
set -euo pipefail

G2G_SRC="${G2G_SRC:-$HOME/src/glass2glass}"
[ -d "$G2G_SRC" ] || { echo "g2g source not at $G2G_SRC (set G2G_SRC)" >&2; exit 1; }
rustup component add miri --toolchain nightly >/dev/null 2>&1 || true

cd "$G2G_SRC"
MIRIFLAGS="${MIRIFLAGS:--Zmiri-disable-isolation}" \
    cargo +nightly miri test -p g2g-core --features std,multi-thread "$@"
