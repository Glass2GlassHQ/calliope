#!/bin/sh
# runs a second mediamtx, a gst-rtsp-server relay and a feeder in this container's network namespace, with the
# fault schedule's netem on the reader ports
set -u

clips=/usr/share/g2g-canary/clips
config=/etc/canary/mediamtx-faulted.yml
schedule=/etc/canary/fault-schedule.txt
gst_relay_port=19555
# reader-facing ports: mediamtx rtsp and hls, the gst-rtsp-server relay, and the g2g srt and rist listeners the feeder calls
faulted_ports="19554 $gst_relay_port 19888 18892 18896 18897"
retry_secs=1
mediamtx_pid_file=/tmp/mediamtx.pid
# a netem change keeps the previous rate and reorder unless it sets them
clean_netem="delay 0ms rate 0bit reorder 0%"

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

# bytes the netem band has sent since setup
band_bytes() {
    tc -s qdisc show dev lo | awk '$1 == "qdisc" { band = $3 == "40:" } band && $1 == "Sent" { print $2; exit }'
}

# each pass applies the window the clock is in and sleeps to its end, so calliope knows the windows from the clock
run_schedule() {
    cycle_secs=$(awk -F: '$1 !~ /^#/ && NF { split($1, field, " "); total += field[2] } END { print total }' "$schedule")
    measured_bytes=
    measured_at=
    while true; do
        now=$(date +%s)
        position=$((now % cycle_secs))
        window_start=0
        # the fault's name after the colon is for the status page
        while read -r line; do
            set -- ${line%%:*}
            case ${1:-#} in '#'*) continue ;; esac
            kind=$1
            secs=$2
            shift 2
            arguments=$*
            [ "$position" -lt $((window_start + secs)) ] && break
            window_start=$((window_start + secs))
        done <"$schedule"
        case $kind in
        clean)
            fault $clean_netem
            measured_bytes=$(band_bytes)
            measured_at=$now
            ;;
        netem)
            fault $arguments
            ;;
        cap)
            if [ -z "$measured_at" ] || [ "$now" -le "$measured_at" ]; then
                echo "$(date -u +%FT%TZ) no clean window measured before this cap, the link stays clean" >&2
                fault $clean_netem
            else
                rate=$(awk -v bytes=$(($(band_bytes) - measured_bytes)) -v secs=$((now - measured_at)) \
                    -v multiple="$arguments" 'BEGIN { printf "%d", bytes * 8 / secs * multiple }')
                fault rate "${rate}bit"
            fi
            ;;
        restart)
            fault $clean_netem
            echo "$(date -u +%FT%TZ) mediamtx scheduled restart"
            kill "$(cat "$mediamtx_pid_file")"
            ;;
        *)
            echo "unknown window kind '$kind' in $schedule" >&2
            exit 1
            ;;
        esac
        window_end=$((now - position + window_start + secs))
        # to the window's end on the clock, so a late wake never skips a one second window
        sleep "$(date +%s.%N | awk -v end="$window_end" '{ left = end - $1; printf "%.3f", (left > 0 ? left : 0) }')"
    done
}

run_mediamtx() {
    while true; do
        echo "$(date -u +%FT%TZ) mediamtx start"
        mediamtx "$config" &
        echo $! >"$mediamtx_pid_file"
        wait $!
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

looped() {
    echo -re -stream_loop -1 -i "$clips/$1"
}

publish() {
    ffmpeg -hide_banner -loglevel warning "$@"
}

setup_netem || exit 1
trap "kill 0" TERM INT
run_mediamtx &
# rtmp ingest is not a faulted port, so the faults hit only what the readers see
forever "rtmp publish" publish $(looped baseline.mp4) -c copy -f flv rtmp://127.0.0.1:19935/faulted &
forever "rtmp publish av" publish $(looped baseline.mp4) $(looped aac.m4a) -c copy -f flv \
    rtmp://127.0.0.1:19935/faulted-av &
forever "srt push" publish $(looped baseline.mp4) -c copy -f mpegts "srt://127.0.0.1:18892?mode=caller" &
# fedora's ffmpeg has no librist, so g2g sends the rist leg, reading the clean mediamtx through pasta
forever "rist push" g2g-launch -q rtspsrc location=rtsp://127.0.0.1:18554/baseline ! h264parse ! mpegtsmux \
    ! ristsink address=127.0.0.1 port=18896 &
forever "gst relay" python3 -I /usr/local/bin/canary-gst-rtsp-relay.py "$gst_relay_port" &
run_schedule
