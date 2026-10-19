#!/usr/bin/env bash
# runs cargo-mutants on the g2g commits since the last finished run, started by deploy/mutants/g2g-mutants.timer
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
MUTANTS_HOME="${MUTANTS_HOME:-$HOME/g2g-mutants}"
G2G_CLONE="$MUTANTS_HOME/glass2glass"
LAST_FINISHED="$MUTANTS_HOME/last-finished-commit"
G2G_URL=https://github.com/Glass2GlassHQ/glass2glass.git
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

# the plugins test binaries that name a module the diff touches
plugin_test_targets() {
    local diff_from=$1 diff_to=$2 path module
    git -C "$G2G_CLONE" diff --name-only "$diff_from" "$diff_to" -- 'g2g-plugins/src/*.rs' |
        while read -r path
        do
            module=$(basename "$path" .rs)
            if [ "$module" = mod ]
            then
                module=$(basename "$(dirname "$path")")
            fi
            grep -lw "$module" "$G2G_CLONE"/g2g-plugins/tests/*.rs || true
        done | sort -u | while read -r path
        do
            echo "--cargo-arg=--test=$(basename "$path" .rs)"
        done
}

# runs one package's mutants in the diff, prints the cargo-mutants exit status
run_scope() {
    local scope=$1 status=0 mutants
    shift
    if ! mutants=$(cd "$G2G_CLONE" && cargo mutants --list --in-diff "$run_dir/diff.patch" "$@")
    then
        echo 1
        return
    fi
    if [ -z "$mutants" ]
    then
        echo "no mutants in the diff" > "$run_dir/$scope.txt"
        echo 0
        return
    fi
    MUTANTS_RESULTS="$run_dir/$scope" "$HERE/mutants-g2g.sh" "$scope" \
        --in-diff "$run_dir/diff.patch" "$@" > "$run_dir/$scope.console" 2>&1 || status=$?
    echo "$status"
}

# 0 all caught, 2 some missed, 3 some timed out: every other status left mutants untested
run_completed() {
    case $1 in
        0 | 2 | 3) return 0 ;;
        *) return 1 ;;
    esac
}

if [ ! -f "$LAST_FINISHED" ]
then
    echo "write the g2g commit to start from into $LAST_FINISHED" >&2
    exit 1
fi

waited=0
while host_busy
do
    if [ "$waited" -ge "$QUIET_WAIT_LIMIT_SECONDS" ]
    then
        echo "host stayed busy for $((waited / 3600))h, skipping this run"
        exit 0
    fi
    sleep "$QUIET_POLL_SECONDS"
    waited=$((waited + QUIET_POLL_SECONDS))
done

if [ ! -d "$G2G_CLONE/.git" ]
then
    git clone -q "$G2G_URL" "$G2G_CLONE"
fi
git -C "$G2G_CLONE" fetch -q origin master
from=$(cat "$LAST_FINISHED")
to=$(git -C "$G2G_CLONE" rev-parse origin/master)
if [ "$from" = "$to" ]
then
    echo "no g2g commits since $from"
    exit 0
fi
git -C "$G2G_CLONE" checkout -q --detach "$to"

run_dir="$MUTANTS_HOME/runs/$(date +%F-%H%M)-${from:0:8}-${to:0:8}"
mkdir -p "$run_dir"
git -C "$G2G_CLONE" diff "$from" "$to" > "$run_dir/diff.patch"
git -C "$G2G_CLONE" log --oneline "$from..$to" > "$run_dir/commits.txt"

unset CARGO_TARGET_DIR
export G2G_SRC="$G2G_CLONE" MUTANTS_SCRATCH="$run_dir" MUTANTS_LOCK_MODE=exclusive
export MUTANTS_JOBS="${MUTANTS_JOBS:-6}" MUTANTS_JOBSERVER_TASKS=4 CARGO_BUILD_JOBS=4

mapfile -t plugin_tests < <(plugin_test_targets "$from" "$to")
core_status=$(run_scope g2g-core -p g2g-core --features std,multi-thread,runtime,metadata,dyn-slot,tracing \
    -E fuzz_parse -- --lib --tests)
plugins_status=$(run_scope g2g-plugins -p g2g-plugins --features std,metadata,mp4-cenc \
    -E 'fuzz_parse|vp[89]parse\.rs.*is_keyframe' --cargo-arg=--lib "${plugin_tests[@]}")
echo "g2g-core exit $core_status, g2g-plugins exit $plugins_status" | tee "$run_dir/status.txt"

if run_completed "$core_status" && run_completed "$plugins_status"
then
    echo "$to" > "$LAST_FINISHED"
    echo "finished $from..$to, results in $run_dir"
else
    echo "run did not complete, $LAST_FINISHED stays at $from" >&2
    exit 1
fi
