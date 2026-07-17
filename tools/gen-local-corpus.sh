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

# H.265: raw elementary (parse path) and in MP4 (demux path).
ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=duration=2:size=176x144:rate=25" \
    -c:v libx265 -pix_fmt yuv420p -f hevc local-corpus/testsrc-176x144.h265

ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=duration=2:size=176x144:rate=25" \
    -c:v libx265 -pix_fmt yuv420p local-corpus/testsrc-176x144-h265.mp4

# 4:2:2 / 4:4:4 elementary streams: exercise ffprobe geometry + non-4:2:0 chunking.
ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=duration=2:size=160x120:rate=25" \
    -c:v libx264 -pix_fmt yuv422p -f h264 local-corpus/testsrc-160x120-422.h264

ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=duration=2:size=160x120:rate=25" \
    -c:v libx264 -pix_fmt yuv444p -f h264 local-corpus/testsrc-160x120-444.h264

echo "local-corpus ready:"
ls -l local-corpus
