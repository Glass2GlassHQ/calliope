#!/bin/sh
# runs the reference servers the peers rows talk to: live555's rtsp proxy, nginx-rtmp with an ffmpeg feed of the
# baseline clip, and libsrt's srt-live-transmit relaying g2g's srt caller to g2g's srt listener
set -u

clips=/usr/share/g2g-canary/clips
retry_secs=1

forever() {
    label=$1
    shift
    while true; do
        echo "$(date -u +%FT%TZ) $label start"
        "$@"
        sleep "$retry_secs"
    done
}

trap "kill 0" TERM INT
# proxyStream-1 is the clean mediamtx baseline path, proxyStream-2 g2g's rtspserversink, -t asks both for rtp over tcp
forever "live555 proxy" live555ProxyServer -t -p 18570 \
    rtsp://127.0.0.1:18554/baseline rtsp://127.0.0.1:18571/peers &
forever "nginx-rtmp" nginx -c /etc/canary/nginx-peers.conf &
forever "rtmp publish" ffmpeg -hide_banner -loglevel warning -re -stream_loop -1 -i "$clips/baseline.mp4" \
    -c copy -f flv rtmp://127.0.0.1:18936/baseline/baseline &
forever "srt relay" srt-live-transmit "srt://:18950?mode=listener" "srt://127.0.0.1:18951?mode=caller" &
wait
