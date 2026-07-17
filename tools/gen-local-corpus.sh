#!/usr/bin/env bash
# generate small local test vectors with ffmpeg (no network, no licensing).
# raw annex-b elementary streams keep the smoke test about decode, not demux.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p local-corpus

ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=duration=2:size=176x144:rate=25" \
    -c:v libx264 -pix_fmt yuv420p -f h264 local-corpus/testsrc-176x144.h264

ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=duration=2:size=176x144:rate=25" \
    -c:v libx264 -pix_fmt yuv420p -f mpegts local-corpus/testsrc-176x144.ts

echo "local-corpus ready:"
ls -l local-corpus
