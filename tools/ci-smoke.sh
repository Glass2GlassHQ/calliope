#!/usr/bin/env bash
# end-to-end smoke: ffmpeg vs gstreamer on generated streams (differential
# decode + a bit-flip robustness scenario). g2g is excluded here; local runs
# add it via CALLIOPE_G2G_LAUNCH.
set -euo pipefail
cd "$(dirname "$0")/.."

tools/gen-local-corpus.sh
cargo run -p calliope-cli -- engines
cargo run -p calliope-cli -- run \
    scenarios/h264-decode-smoke.toml \
    scenarios/h264-ts-decode.toml \
    scenarios/h264-ts-bitflip.toml \
    --engines ffmpeg,gstreamer --report smoke-report.json
