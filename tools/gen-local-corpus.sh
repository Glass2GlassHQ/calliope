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

# 10-bit HEVC Main10: exercise high-bit-depth (2-byte LE samples) decode + chunking.
ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=duration=2:size=176x144:rate=25" \
    -c:v libx265 -pix_fmt yuv420p10le -f hevc local-corpus/testsrc-176x144-10bit.h265

# AV1 in IVF: g2g's native dav1d decode path (not ffmpeg), the code a g2g bug
# actually lives in. Small + short since AV1 encode is slow. Skipped if the
# encoder is absent, so ci-smoke keeps working without it.
if ffmpeg -hide_banner -encoders 2>/dev/null | grep -q libsvtav1; then
    ffmpeg -nostdin -hide_banner -loglevel error -y \
        -f lavfi -i "testsrc2=duration=1:size=128x128:rate=25" \
        -c:v libsvtav1 -pix_fmt yuv420p local-corpus/testsrc-128x128-av1.ivf
fi

# Resolution-change streams: several fixed-size h264 segments concatenated at the
# Annex-B level. Each segment carries its own SPS/IDR, so a compliant decoder
# switches geometry at the boundary. Exercises the engine's caps / buffer
# renegotiation (its own code), not the codec core. Args are (size dur content)
# triples; content varies so frames differ across the switch.
mk_reschange() {
    local out="local-corpus/$1"
    shift
    : > "$out"
    local tmp
    tmp="$(mktemp)"
    while [ "$#" -ge 3 ]; do
        ffmpeg -nostdin -hide_banner -loglevel error -y \
            -f lavfi -i "$3=size=$1:rate=25" -t "$2" \
            -c:v libx264 -pix_fmt yuv420p -f h264 "$tmp"
        cat "$tmp" >> "$out"
        shift 3
    done
    rm -f "$tmp"
}
mk_reschange res-change-multi.h264 \
    176x144 0.4 testsrc2 320x240 0.4 mandelbrot 128x96 0.4 testsrc2 352x288 0.4 smptebars
# ping-pong: return to an earlier size (renegotiate back, not just forward)
mk_reschange res-change-pingpong.h264 \
    176x144 0.4 testsrc2 320x240 0.4 mandelbrot 176x144 0.4 smptebars

echo "local-corpus ready:"
ls -l local-corpus
