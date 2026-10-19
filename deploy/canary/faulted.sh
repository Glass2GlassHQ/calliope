#!/bin/sh
# runs a second mediamtx and feeder in this container's network namespace, with scheduled netem faults on the reader ports
set -u

clip=/usr/share/g2g-canary/clips/baseline.mp4
config=/etc/canary/mediamtx-faulted.yml
# reader-facing ports: mediamtx rtsp and hls, and the g2g srt and rist listeners the feeder calls
faulted_ports="19554 19888 18892 18896 18897"
restart_every_secs=1200
retry_secs=1

# every packet goes to band 1, the filters move faulted-port packets to band 4, which carries the netem qdisc
setup_netem() {
    tc qdisc add dev lo root handle 1: prio bands 4 priomap 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 || return 1
    tc qdisc add dev lo parent 1:4 handle 40: netem delay 0ms || return 1
    for port in $faulted_ports; do
        tc filter add dev lo parent 1: protocol ip prio 1 u32 match ip sport "$port" 0xffff flowid 1:4 || return 1
        tc filter add dev lo parent 1: protocol ip prio 1 u32 match ip dport "$port" 0xffff flowid 1:4 || return 1
    done
}

fault() {
    echo "$(date -u +%FT%TZ) netem $*"
    tc qdisc change dev lo parent 1:4 handle 40: netem "$@"
}

# a 10 minute cycle: each fault window is followed by a clean one so the readers recover in between
schedule_faults() {
    while true; do
        fault delay 0ms
        sleep 120
        fault loss 3%
        sleep 60
        fault delay 0ms
        sleep 60
        fault delay 80ms 40ms distribution normal
        sleep 60
        fault delay 0ms
        sleep 60
        fault delay 20ms reorder 25% 50%
        sleep 60
        fault delay 0ms
        sleep 60
        fault loss 20% 25%
        sleep 30
        fault delay 0ms
        sleep 90
    done
}

run_mediamtx() {
    while true; do
        echo "$(date -u +%FT%TZ) mediamtx start"
        mediamtx "$config" &
        mediamtx_pid=$!
        sleep "$restart_every_secs"
        echo "$(date -u +%FT%TZ) mediamtx scheduled restart"
        kill "$mediamtx_pid"
        wait "$mediamtx_pid"
        sleep "$retry_secs"
    done
}

forever() {
    label=$1
    shift
    while true; do
        echo "$(date -u +%FT%TZ) $label start"
        "$@"
        sleep "$retry_secs"
    done
}

looped_clip() {
    ffmpeg -hide_banner -loglevel warning -re -stream_loop -1 -i "$clip" -c copy "$@"
}

setup_netem || exit 1
trap "kill 0" TERM INT
schedule_faults &
run_mediamtx &
# rtmp ingest is not a faulted port, so the faults hit only what the readers see
forever "rtmp publish" looped_clip -f flv rtmp://127.0.0.1:19935/faulted &
forever "srt push" looped_clip -f mpegts "srt://127.0.0.1:18892?mode=caller" &
# fedora's ffmpeg has no librist, so g2g sends the rist leg, reading the clean mediamtx through pasta
forever "rist push" g2g-launch -q rtspsrc location=rtsp://127.0.0.1:18554/baseline ! h264parse ! mpegtsmux \
    ! ristsink address=127.0.0.1 port=18896 &
wait
