#!/usr/bin/env bash
# H.264 / H.265 / VP9 conformance of g2g's GPU decoders against the committed baseline, started by deploy/conformance/g2g-conformance.timer
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CALLIOPE="$(dirname "$HERE")"
CONFORMANCE_HOME="${CONFORMANCE_HOME:-$HOME/g2g-conformance}"
G2G_CLONE="$CONFORMANCE_HOME/glass2glass"
G2G_URL=https://github.com/Glass2GlassHQ/glass2glass.git
G2G_FEATURES=std,nvdec,vulkan-video
BACKENDS=(nvdec vulkanvideodec)
H26X_SUITES="fluster/JVT-AVC_V1/ fluster/JCT-VC-HEVC_V1/ fluster/JCT-VC-RExt/"
VP9_SUITES="fluster/VP9-TEST-VECTORS/ fluster/VP9-TEST-VECTORS-HIGH/"
# g2g's vulkan video decoder has no vp9
declare -A BACKEND_SUITES=([nvdec]="$H26X_SUITES $VP9_SUITES" [vulkanvideodec]="$H26X_SUITES")
BASELINE="$CALLIOPE/conformance/rtx2080ti.json"
ISSUE_REPO=Glass2GlassHQ/calliope
ISSUE_TITLE="conformance gate red on hercules"
KEEP_DAYS=14
: "${CONFORMANCE_PUBLISH_DESTINATION:?set it to the s3 url the page goes to}"

# runs one backend over the given id prefixes, writes the calliope report
run_conformance() {
    local backend=$1 report=$2 prefix_args=()
    shift 2
    for prefix in "$@"
    do
        prefix_args+=(--id-prefix "$prefix")
    done
    # calliope exits non-zero whenever a vector fails, the report says which
    G2G_PLUGIN_FEATURE_RANK="$backend:max" CALLIOPE_G2G_LAUNCH="$G2G_CLONE/target/release/g2g-launch" \
        "$CALLIOPE/target/release/calliope" conformance --corpus "$CALLIOPE/corpus/vectors.toml" \
        --engines g2g --jobs 1 --workdir "$run_dir/work-$(basename "$report" .json)" --report "$report" \
        "${prefix_args[@]}" > "${report%.json}.log" 2>&1 || true
    if [ ! -f "$report" ]
    then
        printf '%s\n\ncalliope wrote no report for %s:\n\n```\n%s\n```\n' \
            "$title" "$backend" "$(tail -n 30 "${report%.json}.log")" > "$run_dir/summary.md"
        return 1
    fi
}

# the page goes up after every run that got far enough to write a summary
publish() {
    local page_args=("$run_dir/summary.md")
    if [ -f "$run_dir/retry.json" ]
    then
        page_args+=("$run_dir/table.json" "$run_dir/retry.json")
    fi
    "$HERE/conformance-gate.py" page "$run_dir/index.html" "${page_args[@]}"
    for file in index.html summary.md table.json
    do
        if [ -f "$run_dir/$file" ]
        then
            aws s3 cp --only-show-errors --cache-control max-age=300 \
                "$run_dir/$file" "$CONFORMANCE_PUBLISH_DESTINATION/$file"
        fi
    done
}

# the gate's own open issue, found by title since the account that runs it cannot label issues
open_issue() {
    gh issue list -R "$ISSUE_REPO" --author @me --state open --search "\"$ISSUE_TITLE\" in:title" \
        --json number,title --jq "map(select(.title == \"$ISSUE_TITLE\")) | .[0].number // empty"
}

# one open issue per red stretch: opened on the first red night, commented on later ones
report_red() {
    local open
    open=$(open_issue)
    if [ -n "$open" ]
    then
        gh issue comment -R "$ISSUE_REPO" "$open" -F "$run_dir/summary.md" > /dev/null
    else
        gh issue create -R "$ISSUE_REPO" --title "$ISSUE_TITLE" \
            -F "$run_dir/summary.md" > /dev/null
    fi
    echo "red, summary in $run_dir/summary.md" >&2
    publish
    exit 1
}

report_green() {
    local open
    open=$(open_issue)
    if [ -n "$open" ]
    then
        gh issue close -R "$ISSUE_REPO" "$open" -c "green again at g2g $revision" > /dev/null
    fi
    echo "green, summary in $run_dir/summary.md"
    publish
}

if ! "$HERE/wait-for-quiet-host.sh"
then
    echo "skipping this run"
    exit 0
fi

if [ ! -d "$G2G_CLONE/.git" ]
then
    git clone -q "$G2G_URL" "$G2G_CLONE"
fi
git -C "$G2G_CLONE" fetch -q origin master
git -C "$G2G_CLONE" checkout -q --detach origin/master
revision=$(git -C "$G2G_CLONE" rev-parse --short=8 HEAD)
gpu=$(nvidia-smi --query-gpu=name,driver_version --format=csv,noheader)
title="g2g $revision, calliope $(git -C "$CALLIOPE" rev-parse --short=7 HEAD), $gpu, $(date +%F)"

run_dir="$CONFORMANCE_HOME/runs/$(date +%F-%H%M)-$revision"
mkdir -p "$run_dir"
ln -sfn "$run_dir" "$CONFORMANCE_HOME/latest"
find "$CONFORMANCE_HOME/runs" -mindepth 1 -maxdepth 1 -mtime +"$KEEP_DAYS" -exec rm -rf {} +

unset CARGO_TARGET_DIR
if ! (cd "$G2G_CLONE" && cargo build --release --locked -p g2g-plugins --bin g2g-launch \
    --features "$G2G_FEATURES") > "$run_dir/build-g2g.log" 2>&1
then
    printf '%s\n\ng2g-launch does not build with `%s`:\n\n```\n%s\n```\n' \
        "$title" "$G2G_FEATURES" "$(tail -n 30 "$run_dir/build-g2g.log")" > "$run_dir/summary.md"
    report_red
fi
if ! (cd "$CALLIOPE" && cargo build --release --locked -p calliope-cli) > "$run_dir/build-calliope.log" 2>&1
then
    printf '%s\n\ncalliope does not build:\n\n```\n%s\n```\n' \
        "$title" "$(tail -n 30 "$run_dir/build-calliope.log")" > "$run_dir/summary.md"
    report_red
fi

table_args=()
for backend in "${BACKENDS[@]}"
do
    # the suite list splits into one prefix per word
    run_conformance "$backend" "$run_dir/report-$backend.json" ${BACKEND_SUITES[$backend]} || report_red
    table_args+=("$backend=$run_dir/report-$backend.json")
done
"$HERE/conformance-gate.py" table "$run_dir/table.json" "${table_args[@]}"

if [ ! -f "$BASELINE" ]
then
    echo "no baseline at $BASELINE, review $run_dir/table.json and commit it there"
    exit 0
fi

retry_args=()
for backend in "${BACKENDS[@]}"
do
    mapfile -t vectors < <("$HERE/conformance-gate.py" differing "$BASELINE" "$run_dir/table.json" |
        awk -v backend="$backend" '$1 == backend { print $2 }')
    if [ "${#vectors[@]}" -gt 0 ]
    then
        run_conformance "$backend" "$run_dir/retry-$backend.json" "${vectors[@]}" || report_red
        retry_args+=("$backend=$run_dir/retry-$backend.json")
    fi
done
if [ "${#retry_args[@]}" -gt 0 ]
then
    "$HERE/conformance-gate.py" table "$run_dir/retry.json" "${retry_args[@]}"
else
    echo '{}' > "$run_dir/retry.json"
fi

# decoded output is large, the logs stay for the summary's failures
find "$run_dir" -path "$run_dir/work-*" -type f ! -name '*.log' -delete

if "$HERE/conformance-gate.py" verdict "$BASELINE" "$run_dir/table.json" "$run_dir/retry.json" \
    "$run_dir/summary.md" "$title"
then
    report_green
else
    report_red
fi
