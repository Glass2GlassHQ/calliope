#!/usr/bin/env bash
# build g2g-launch with AddressSanitizer for fuzzing under calliope. ASan
# intercepts malloc process-wide, so heap-buffer-overflow / use-after-free in
# g2g's own Rust code and in the system libav it calls are both caught. Point
# CALLIOPE_G2G_LAUNCH at the result and run tools/fuzz-g2g.sh.
#
# needs nightly (for -Zsanitizer) and the rust-src component (for -Zbuild-std,
# which rebuilds std with ASan so allocations inside std are instrumented too).
set -euo pipefail

G2G_SRC="${G2G_SRC:-$HOME/src/glass2glass}"
OUT="${OUT:-$HOME/.local/bin/g2g-launch-asan}"
TARGET=x86_64-unknown-linux-gnu
# decode-focused; add dav1d,mjpeg for AV1 / JPEG (needs libdav1d-dev)
FEATURES="${G2G_FEATURES:-ffmpeg,multi-thread}"

[ -d "$G2G_SRC" ] || { echo "g2g source not at $G2G_SRC (set G2G_SRC)" >&2; exit 1; }
rustup component add rust-src --toolchain nightly >/dev/null 2>&1 || true

echo "building g2g-launch (asan) from $G2G_SRC [features: $FEATURES]"
cd "$G2G_SRC"
RUSTFLAGS="-Zsanitizer=address" \
    cargo +nightly build -Z build-std \
    --target "$TARGET" \
    -p g2g-plugins --bin g2g-launch \
    --no-default-features --features "$FEATURES"

mkdir -p "$(dirname "$OUT")"
cp "target/$TARGET/debug/g2g-launch" "$OUT"
echo "asan g2g-launch -> $OUT"
echo "run: CALLIOPE_G2G_LAUNCH=$OUT $(dirname "$0")/fuzz-g2g.sh"
