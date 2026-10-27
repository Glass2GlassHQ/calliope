#!/bin/sh
# generates the clips, publishes them to mediamtx over rtsp and pushes them to the g2g srt and rist listeners
# `feeder.sh clips <dir>` only generates and checks the clips, the canary image build hashes the same files
set -u

rtsp_base=rtsp://127.0.0.1:18554
srt_url="srt://127.0.0.1:18890?mode=caller"
rist_url=rist://127.0.0.1:18894
retry_secs=1

# 16 seconds holds a whole number of 30 fps frames and 1024-sample aac frames, so audio and video loop together
clip_secs=16
testsrc="-f lavfi -i testsrc2=rate=30"
tone="-f lavfi -i sine=frequency=440:sample_rate=48000"

# simd code rounds differently across cpus, so the clips are encoded with plain c code on one thread
# and carry no random container ids, and every machine writes the same bytes
clip_md5s="
c158a2f515cc5b360f5829cafd75ad56  baseline.mp4
7db5109fea59220c11e7f38cd77c8167  hevc.mp4
66fea357a505a4c538588278d20045d0  av1.mp4
9a1b15b49a69d6aa1da83a86d68f5ec8  vp9.webm
ae6fafb7453b94d1b9a48678ffdffc61  switching.ts
a03889c14c69c68bd2c5fb2891ad79f5  abr-720.mp4
e14cc08922d125ad63639e32a13bcd97  abr-360.mp4
f4ea8f22c2ea9eda0aafe9d2636d737d  abr-180.mp4
"

encode() {
    encode_secs "$clip_secs" "$@"
}

encode_secs() {
    secs=$1
    output=$2
    shift 2
    partial="$output.partial.${output##*.}"
    ffmpeg -hide_banner -loglevel error -cpuflags 0 "$@" -t "$secs" -fflags +bitexact -flags +bitexact \
        -y "$partial" && mv "$partial" "$output"
}

# no scenecut keyframes, so every part and variant of a clip has a keyframe every -g frames and no others
x264_gop_aligned="-c:v libx264 -preset veryfast -bf 0 -sc_threshold 0 -threads 1 -x264-params asm=0 -pix_fmt yuv420p"

# three sizes and two frame rates in a row, each part starting on an idr with its own sps, which mpeg-ts carries in band
make_switching_clip() {
    dir=$1
    encode_secs 4 "$dir/switching-0.ts" $testsrc:size=640x360 $x264_gop_aligned -g 30 || return 1
    encode_secs 4 "$dir/switching-1.ts" $testsrc:size=1280x720 $x264_gop_aligned -g 30 || return 1
    encode_secs 4 "$dir/switching-2.ts" -f lavfi -i testsrc2=rate=25:size=320x240 $x264_gop_aligned -g 25 || return 1
    printf "file switching-%s.ts\n" 0 1 2 >"$dir/switching-parts.txt"
    encode_secs 12 "$dir/switching.ts" -f concat -i "$dir/switching-parts.txt" -c copy
    rm -f "$dir"/switching-?.ts "$dir/switching-parts.txt"
}

make_clips() {
    dir=$1
    mkdir -p "$dir"
    # libaom and libvpx read these masks, a zero mask turns their simd code off
    export AOM_SIMD_CAPS_MASK=0 VPX_SIMD_CAPS_MASK=0
    # mediamtx closes a webrtc session on h264 with b-frames
    encode "$dir/baseline.mp4" $testsrc:size=640x360 \
        -c:v libx264 -preset veryfast -bf 0 -g 30 -threads 1 -x264-params asm=0 -pix_fmt yuv420p
    encode "$dir/hevc.mp4" $testsrc:size=640x360 -c:v libx265 -preset ultrafast \
        -x265-params pools=none:frame-threads=1:asm=none:keyint=30:bframes=0:log-level=error -pix_fmt yuv420p
    encode "$dir/av1.mp4" $testsrc:size=640x360 -c:v libaom-av1 -usage realtime \
        -cpu-used 8 -threads 1 -row-mt 0 -g 30 -b:v 800k -pix_fmt yuv420p
    encode "$dir/vp9.webm" $testsrc:size=640x360 -c:v libvpx-vp9 -deadline realtime \
        -cpu-used 8 -threads 1 -row-mt 0 -g 30 -b:v 800k -pix_fmt yuv420p
    # libopus picks its simd code at run time, so the audio clips are not in the md5 list
    encode "$dir/aac.m4a" $tone -ac 2 -c:a aac -b:a 96k
    encode "$dir/opus.ogg" $tone -ac 2 -c:a libopus -b:a 64k
    make_switching_clip "$dir"
    # the faulted container serves these as one multi-variant hls stream for the abr row
    encode "$dir/abr-720.mp4" $testsrc:size=1280x720 $x264_gop_aligned -g 30 -b:v 3000k -maxrate 3000k -bufsize 3000k
    encode "$dir/abr-360.mp4" $testsrc:size=640x360 $x264_gop_aligned -g 30 -b:v 1000k -maxrate 1000k -bufsize 1000k
    encode "$dir/abr-180.mp4" $testsrc:size=320x180 $x264_gop_aligned -g 30 -b:v 300k -maxrate 300k -bufsize 300k
    clips_match "$dir"
}

clips_match() {
    (cd "$1" && [ -s aac.m4a ] && [ -s opus.ogg ] && echo "$clip_md5s" | grep . | md5sum -c -s -)
}

if [ "${1:-}" = clips ]; then
    make_clips "${2:?usage: feeder.sh clips <dir>}"
    exit
fi

clip=/clip
if ! clips_match "$clip" 2>/dev/null; then
    make_clips "$clip" || exit 1
fi

forever() {
    label=$1
    shift
    while true; do
        echo "$(date -u +%FT%TZ) $label start"
        ffmpeg -hide_banner -loglevel warning "$@"
        sleep "$retry_secs"
    done
}

looped() {
    echo -re -stream_loop -1 -i "$clip/$1"
}

# ffmpeg marks av1 and vp9 rtp packetizing experimental
publish() {
    path=$1
    shift
    forever "rtsp publish $path" "$@" -c copy -strict experimental \
        -f rtsp -rtsp_transport tcp "$rtsp_base/$path"
}

trap "kill 0" TERM INT
publish baseline $(looped baseline.mp4) &
publish hevc $(looped hevc.mp4) &
publish av1 $(looped av1.mp4) &
publish vp9-opus $(looped vp9.webm) $(looped opus.ogg) &
publish h264-aac $(looped baseline.mp4) $(looped aac.m4a) &
publish switching $(looped switching.ts) &
# g2g's srtsrc and ristsrc only listen, so the feeder calls them and reconnects when g2g restarts
forever "srt push" -rtsp_transport tcp -i "$rtsp_base/baseline" -c copy -f mpegts "$srt_url" &
forever "rist push" $(looped baseline.mp4) -c copy -f mpegts -rist_profile simple "$rist_url" &
wait
