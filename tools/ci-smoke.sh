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
    scenarios/h264-autoprobe.toml \
    scenarios/h264-422-autoprobe.toml \
    scenarios/h264-444-autoprobe.toml \
    scenarios/h265-decode.toml \
    scenarios/h265-mp4-decode.toml \
    scenarios/h264-ts-bitflip.toml \
    scenarios/h264-outcome-diff.toml \
    scenarios/opus-decode-diff.toml \
    scenarios/aac-determinism.toml \
    --engines ffmpeg,gstreamer --report smoke-report.json
