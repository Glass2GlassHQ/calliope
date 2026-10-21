#!/usr/bin/env bash
# blocks until hercules is quiet enough for a scheduled job, exits 1 if it stays busy past the limit
set -euo pipefail

# a 5 minute load average of this or more means another job is using the box
MAX_IDLE_LOAD="${MAX_IDLE_LOAD:-4}"
# the host-validation timer starts its run ten minutes after boot
BOOT_SETTLE_SECONDS=1200
QUIET_POLL_SECONDS=300
QUIET_WAIT_LIMIT_SECONDS=$((8 * 3600))

host_busy() {
    local uptime_seconds load_five_minutes
    uptime_seconds=$(cut -d. -f1 /proc/uptime)
    if [ "$uptime_seconds" -lt "$BOOT_SETTLE_SECONDS" ]
    then
        return 0
    fi
    if [ -n "$(systemctl --user list-units --state=active --plain --no-legend 'glass2glass-validation@*.service')" ]
    then
        return 0
    fi
    load_five_minutes=$(cut -d' ' -f2 /proc/loadavg)
    [ "${load_five_minutes%.*}" -ge "$MAX_IDLE_LOAD" ]
}

waited=0
while host_busy
do
    if [ "$waited" -ge "$QUIET_WAIT_LIMIT_SECONDS" ]
    then
        echo "host stayed busy for $((waited / 3600))h" >&2
        exit 1
    fi
    sleep "$QUIET_POLL_SECONDS"
    waited=$((waited + QUIET_POLL_SECONDS))
done
