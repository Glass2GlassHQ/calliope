#!/usr/bin/env bash
# fuzz g2g's decoder: inject seeded corruption into real streams and require
# graceful degradation (no crash / hang). Run against an ASan build (see
# tools/build-g2g-asan.sh) so memory bugs abort loudly instead of corrupting
# silently. Every crash / hang is shrunk to a minimal reproducer under fuzz-out/.
#
# env: FUZZ_SEEDS (per input, default 50), FUZZ_MODE (nal-payload|bit-flip|
# truncate|byte-drop), FUZZ_COUNT (ops per run), FUZZ_OUT (findings dir).
# extra args are added to the input list.
set -euo pipefail
cd "$(dirname "$0")/.."

: "${CALLIOPE_G2G_LAUNCH:?point it at an asan g2g-launch (run tools/build-g2g-asan.sh)}"
export CALLIOPE_G2G_LAUNCH
# ASan must abort (SIGABRT) on error so calliope classifies it as a crash, not a
# clean exit; codec libs leak intentionally on abort, so skip leak reports.
export ASAN_OPTIONS="${ASAN_OPTIONS:-abort_on_error=1:detect_leaks=0}"

SEEDS="${FUZZ_SEEDS:-50}"
MODE="${FUZZ_MODE:-nal-payload}"
COUNT="${FUZZ_COUNT:-500}"
OUT="${FUZZ_OUT:-fuzz-out}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$OUT"

[ -d local-corpus ] || tools/gen-local-corpus.sh
mapfile -t INPUTS < <(ls local-corpus/*.h264 local-corpus/*.h265 local-corpus/*.ts \
    local-corpus/*.mp4 local-corpus/*.ivf 2>/dev/null || true)
[ "$#" -gt 0 ] && INPUTS+=("$@")
[ "${#INPUTS[@]}" -gt 0 ] || { echo "no inputs (generate local-corpus or pass files)" >&2; exit 1; }

cargo build -q -p calliope-cli
BIN=target/debug/calliope

runs=0; found=0
for input in "${INPUTS[@]}"; do
    abs="$(readlink -f "$input")"
    for seed in $(seq 1 "$SEEDS"); do
        id="fuzz-$(basename "$input")-$seed"
        scn="$WORK/$id.toml"
        cat > "$scn" <<EOF
id = "$id"
engines = ["g2g"]
reference = "g2g"
timeout-secs = 20
[input]
path = "$abs"
[fault]
mode = "$MODE"
seed = $seed
count = $COUNT
EOF
        runs=$((runs + 1))
        if ! "$BIN" run "$scn" --workdir "$WORK/runs" >/dev/null 2>&1; then
            found=$((found + 1))
            corrupted="$WORK/runs/$id/input.corrupted"
            echo "CRASH/HANG: $input seed=$seed mode=$MODE"
            if ! "$BIN" minimize --engine g2g --input "$corrupted" \
                --out "$OUT/$id.min" --timeout-secs 20 >/dev/null 2>&1; then
                cp "$corrupted" "$OUT/$id.corrupted"
            fi
            cp "$WORK/runs/$id/g2g/stderr.log" "$OUT/$id.stderr" 2>/dev/null || true
        fi
    done
done
echo "fuzz done: $runs runs, $found finding(s) -> $OUT/"
