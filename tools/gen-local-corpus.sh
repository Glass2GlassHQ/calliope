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
# capture first: piping straight into grep -q sigpipes ffmpeg, which pipefail
# then reads as "encoder absent".
encoders="$(ffmpeg -hide_banner -encoders 2>/dev/null)"
if grep -q libsvtav1 <<<"$encoders"; then
    ffmpeg -nostdin -hide_banner -loglevel error -y \
        -f lavfi -i "testsrc2=duration=1:size=128x128:rate=25" \
        -c:v libsvtav1 -pix_fmt yuv420p local-corpus/testsrc-128x128-av1.ivf

    # same stream in MP4: av01 sample-entry demux path.
    ffmpeg -nostdin -hide_banner -loglevel error -y \
        -f lavfi -i "testsrc2=duration=1:size=128x128:rate=25" \
        -c:v libsvtav1 -pix_fmt yuv420p local-corpus/testsrc-128x128-av1.mp4
fi

# H.264 in Matroska: mkv demux plus avcc to annex-b conversion.
ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=duration=2:size=176x144:rate=25" \
    -c:v libx264 -pix_fmt yuv420p local-corpus/testsrc-176x144.mkv

# Interlaced: field-coded pictures (interlaced dct + motion estimation, top
# field first), so the interlace signal has to survive parse, decode and caps.
# mpeg-2 in a dvd-style program stream and in mpeg-ts, plus tff h264.
ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=duration=2:size=352x288:rate=25" \
    -c:v mpeg2video -flags +ildct+ilme -top 1 -b:v 3M \
    -f vob local-corpus/interlaced-352x288.mpg

ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=duration=2:size=352x288:rate=25" \
    -c:v mpeg2video -flags +ildct+ilme -top 1 -b:v 3M \
    -f mpegts local-corpus/interlaced-352x288-mpeg2.ts

ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=duration=2:size=352x288:rate=25" \
    -c:v libx264 -pix_fmt yuv420p -flags +ildct+ilme -x264-params tff=1 \
    -f h264 local-corpus/interlaced-352x288.h264

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

# --- audio ---
# Opus decodes bit-exactly across libopus-backed engines, so it feeds the
# cross-engine differential. sine (mono) + a two-tone (stereo) less-trivial
# case; opus always decodes at 48k.
ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "sine=frequency=440:duration=2:sample_rate=48000" \
    -c:a libopus -ac 1 local-corpus/tone-mono-48k.opus

ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "aevalsrc=exprs='0.4*sin(440*2*PI*t)|0.4*sin(660*2*PI*t)':s=48000:d=2" \
    -c:a libopus -ac 2 local-corpus/twotone-stereo-48k.opus

# AAC is not bit-exact across decoders, so it is determinism-only (a
# self-comparison). sine + pink-noise, in mpeg-ts at 44.1k stereo.
ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "sine=frequency=440:duration=2:sample_rate=44100" \
    -c:a aac -ac 2 -f mpegts local-corpus/tone-stereo-44k-aac.ts

ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "anoisesrc=d=2:color=pink:sample_rate=44100" \
    -c:a aac -ac 2 -f mpegts local-corpus/noise-stereo-44k-aac.ts

# Chained ogg: two logical opus streams concatenated, so a decoder has to
# rebuild at the chain boundary instead of stopping after the first link.
# (-f ogg names the muxer explicitly so the temp file needs no .opus suffix;
# macOS mktemp has no --suffix)
chain_tmp="$(mktemp)"
ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "sine=frequency=660:duration=2:sample_rate=48000" \
    -c:a libopus -ac 1 -f ogg "$chain_tmp"
cat local-corpus/tone-mono-48k.opus "$chain_tmp" > local-corpus/chained-mono-48k.opus
rm -f "$chain_tmp"

# FLAC: lossless integer decode, bit-exact everywhere. Native container and
# ogg-flac (the ogg demux path to the same decoder).
ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "sine=frequency=440:duration=2:sample_rate=48000" \
    -c:a flac -ac 1 local-corpus/tone-mono-48k.flac

ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "sine=frequency=440:duration=2:sample_rate=48000" \
    -c:a flac -ac 1 -f ogg local-corpus/tone-mono-48k-flac.oga

# Ogg-Vorbis. libvorbis, since ffmpeg's native vorbis encoder is stereo-only.
ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "sine=frequency=440:duration=2:sample_rate=44100" \
    -c:a libvorbis -ac 1 local-corpus/tone-mono-44k.ogg

# Legacy broadcast audio in mpeg-ts: ac3 and mp2.
ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "aevalsrc=exprs='0.4*sin(440*2*PI*t)|0.4*sin(660*2*PI*t)':s=48000:d=2" \
    -c:a ac3 -b:a 192k -f mpegts local-corpus/tone-stereo-48k-ac3.ts

ffmpeg -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i "aevalsrc=exprs='0.4*sin(440*2*PI*t)|0.4*sin(660*2*PI*t)':s=48000:d=2" \
    -c:a mp2 -f mpegts local-corpus/tone-stereo-48k-mp2.ts

echo "local-corpus ready:"
ls -l local-corpus
