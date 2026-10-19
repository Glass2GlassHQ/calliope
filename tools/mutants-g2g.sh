#!/usr/bin/env bash
# usage: mutants-g2g.sh <scope> [cargo-mutants args...], results land in mutants/<scope>/
set -euo pipefail

if [ $# -lt 1 ]
then
    echo "usage: $0 <scope> [cargo-mutants args...]" >&2
    exit 1
fi
SCOPE="$1"
shift

G2G_SRC="${G2G_SRC:-$HOME/src/glass2glass}"
if [ ! -d "$G2G_SRC" ]
then
    echo "g2g source not at $G2G_SRC (set G2G_SRC)" >&2
    exit 1
fi
if ! command -v cargo-mutants >/dev/null
then
    echo "cargo install --locked cargo-mutants" >&2
    exit 1
fi

CALLIOPE="$(cd "$(dirname "$0")/.." && pwd)"
RESULTS="${MUTANTS_RESULTS:-$CALLIOPE/mutants/$SCOPE}"
# scratch tree copies and logs stay out of the repo and off the tmpfs /tmp
SCRATCH="${MUTANTS_SCRATCH:-$HOME/tmp/calliope-mutants}"
OUT="$SCRATCH/out/$SCOPE"
TIMEOUT_MULTIPLIER="${TIMEOUT_MULTIPLIER:-3}"
mkdir -p "$SCRATCH/tmp" "$OUT" "$RESULTS"

# manual runs share the lock, mutants-scheduled.sh takes it exclusively
LOCK="$HOME/tmp/calliope-mutants.lock"
mkdir -p "$(dirname "$LOCK")"
exec 9>"$LOCK"
if [ "${MUTANTS_LOCK_MODE:-shared}" = exclusive ]
then
    lock_flag=-x
else
    lock_flag=-s
fi
if ! flock "$lock_flag" -n 9
then
    echo "another mutants run holds $LOCK" >&2
    exit 1
fi

# the defaults suit a shared desktop, which crashed under more; a quiet box takes more
MUTANTS_JOBS="${MUTANTS_JOBS:-1}"
MUTANTS_JOBSERVER_TASKS="${MUTANTS_JOBSERVER_TASKS:-2}"
# a mutated loop in a const block keeps rustc evaluating it until the build times out
MUTANTS_ARGS=(--jobs "$MUTANTS_JOBS" --jobserver-tasks "$MUTANTS_JOBSERVER_TASKS" --cap-lints true
    --timeout-multiplier "$TIMEOUT_MULTIPLIER" --build-timeout-multiplier 5 --colors never "$@")

start=$(date +%s)
set +e
cd "$G2G_SRC"
TMPDIR="$SCRATCH/tmp" CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}" nice -n 10 \
    cargo mutants --output "$OUT" "${MUTANTS_ARGS[@]}" 2>&1 | tee "$OUT/run.log"
status=${PIPESTATUS[0]}
set -e
wall=$(( $(date +%s) - start ))

for list in caught missed timeout unviable
do
    cp "$OUT/mutants.out/$list.txt" "$RESULTS/$list.txt"
done
{
    echo "command: cargo mutants ${MUTANTS_ARGS[*]}"
    echo "g2g: $(git -C "$G2G_SRC" rev-parse --short HEAD)"
    echo "cargo-mutants: $(cargo mutants --version)"
    echo "wall: $((wall / 3600))h $((wall % 3600 / 60))m"
    echo "exit: $status"
    grep -E '^[0-9]+ mutants tested' "$OUT/run.log" | tail -1
} > "$RESULTS/summary.txt"
cat "$RESULTS/summary.txt"
exit "$status"
