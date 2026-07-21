#!/usr/bin/env bash
# Build g2g-launch under ThreadSanitizer and run a multi-threaded decode, to
# catch data races Miri's single interleaving and ASan cannot see. TSan needs
# the binary to actually run threads, so this drives g2g's `--threads` decode
# over real streams (the determinism scenario's threaded variant). Complements
# loom (the SPSC ring, exhaustive) and Miri (g2g-core only, one interleaving):
# TSan instruments the whole process including the C libav it calls.
#
# needs nightly + rust-src (for -Zbuild-std, so std is TSan-instrumented too).
set -euo pipefail

G2G_SRC="${G2G_SRC:-$HOME/src/glass2glass}"
OUT="${OUT:-$HOME/.local/bin/g2g-launch-tsan}"
TARGET=x86_64-unknown-linux-gnu
FEATURES="${G2G_FEATURES:-ffmpeg,multi-thread}"

[ -d "$G2G_SRC" ] || { echo "g2g source not at $G2G_SRC (set G2G_SRC)" >&2; exit 1; }
rustup component add rust-src --toolchain nightly >/dev/null 2>&1 || true

echo "building g2g-launch (tsan) from $G2G_SRC [features: $FEATURES]"
( cd "$G2G_SRC" && RUSTFLAGS="-Zsanitizer=thread" \
    cargo +nightly build -Z build-std \
    --target "$TARGET" \
    -p g2g-plugins --bin g2g-launch \
    --no-default-features --features "$FEATURES" )
mkdir -p "$(dirname "$OUT")"
cp "$G2G_SRC/target/$TARGET/debug/g2g-launch" "$OUT"
echo "tsan g2g-launch -> $OUT"

cd "$(dirname "$0")/.."
CALLIOPE_G2G_LAUNCH="$OUT" \
TSAN_OPTIONS="${TSAN_OPTIONS:-halt_on_error=1}" \
    cargo run --release -p calliope-cli -- \
    run scenarios/g2g-determinism.toml --engines g2g
