#!/bin/sh
# publishes a looped clip to mediamtx over rtsp and pushes it to the g2g srt listener
set -u

clip=/clip/baseline-no-bframes.mp4
rtsp_url=rtsp://127.0.0.1:18554/baseline
srt_url="srt://127.0.0.1:18890?mode=caller"
retry_secs=1

if [ ! -s "$clip" ]; then
    mkdir -p /clip
    # mediamtx closes a webrtc session on h264 with b-frames
    ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc2=size=1280x720:rate=30 -t 20 \
        -c:v libx264 -preset veryfast -bf 0 -g 30 -pix_fmt yuv420p -y "$clip.partial.mp4"
    mv "$clip.partial.mp4" "$clip"
fi

publish() {
    while true; do
        echo "$(date -u +%FT%TZ) rtsp publish start"
        ffmpeg -hide_banner -loglevel warning -re -stream_loop -1 -i "$clip" -c copy \
            -f rtsp -rtsp_transport tcp "$rtsp_url"
        sleep "$retry_secs"
    done
}

# g2g's srtsrc only listens, so the feeder is the srt caller and reconnects when g2g restarts
push_srt() {
    while true; do
        echo "$(date -u +%FT%TZ) srt push start"
        ffmpeg -hide_banner -loglevel warning -rtsp_transport tcp -i "$rtsp_url" -c copy \
            -f mpegts "$srt_url"
        sleep "$retry_secs"
    done
}

trap "kill 0" TERM INT
publish &
push_srt &
wait
