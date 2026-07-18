#!/usr/bin/env bash
# corrupt-input differential fuzz: inject seeded corruption into real streams,
# decode on ffmpeg (the trusted oracle) and g2g, and cross-compare decode
# *outcomes*. A pixel compare is meaningless on corrupt input (error concealment
# is implementation-defined), so the signal is structural:
#   - CRASH/HANG   g2g (or ffmpeg) died on a signal / timed out (hardening bug)
#   - LENIENT      g2g decoded a stream ffmpeg refused (too-lenient parser: the
#                  class both known g2g bugs came from; the headline finding)
#   - stricter     g2g refused a stream ffmpeg decoded (interop, lower value)
# Findings land under OUT/; each keeps the corrupted input for repro. Run g2g
# under an ASan build (tools/build-g2g-asan.sh) so memory bugs abort loudly.
#
# env: FUZZ_SEEDS (per input, default 50), FUZZ_MODE (nal-payload|bit-flip|
# truncate|byte-drop), FUZZ_COUNT (ops per run), FUZZ_OUT (findings dir).
# extra args are added to the input list.
set -euo pipefail
cd "$(dirname "$0")/.."

: "${CALLIOPE_G2G_LAUNCH:?point it at a g2g-launch (feature or asan build)}"
export CALLIOPE_G2G_LAUNCH
export ASAN_OPTIONS="${ASAN_OPTIONS:-abort_on_error=1:detect_leaks=0}"

SEEDS="${FUZZ_SEEDS:-50}"
MODE="${FUZZ_MODE:-nal-payload}"
COUNT="${FUZZ_COUNT:-500}"
OUT="${FUZZ_OUT:-outcome-diff-out}"
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

runs=0; crashes=0; lenient=0; stricter=0
for input in "${INPUTS[@]}"; do
    abs="$(readlink -f "$input")"
    for seed in $(seq 1 "$SEEDS"); do
        id="od-$(basename "$input")-$seed"
        scn="$WORK/$id.toml"
        cat > "$scn" <<EOF
id = "$id"
engines = ["ffmpeg", "g2g"]
reference = "ffmpeg"
timeout-secs = 20
outcome-diff = true
[input]
path = "$abs"
[fault]
mode = "$MODE"
seed = $seed
count = $COUNT
EOF
        runs=$((runs + 1))
        log="$WORK/$id.log"
        corrupted="$WORK/runs/$id/input.corrupted"
        if ! "$BIN" run "$scn" --workdir "$WORK/runs" >"$log" 2>&1; then
            # a crash / hang fails the run; save + minimize the reproducer
            crashes=$((crashes + 1))
            echo "CRASH/HANG: $input seed=$seed mode=$MODE"
            if ! "$BIN" minimize --engine g2g --input "$corrupted" \
                --out "$OUT/$id.min" --timeout-secs 20 >/dev/null 2>&1; then
                cp "$corrupted" "$OUT/$id.corrupted" 2>/dev/null || true
            fi
            cp "$WORK/runs/$id/g2g/stderr.log" "$OUT/$id.stderr" 2>/dev/null || true
            continue
        fi
        if grep -q "LENIENT" "$log"; then
            lenient=$((lenient + 1))
            echo "LENIENT: $input seed=$seed mode=$MODE (g2g decoded, ffmpeg rejected)"
            cp "$corrupted" "$OUT/$id.lenient" 2>/dev/null || true
        elif grep -q "stricter" "$log"; then
            stricter=$((stricter + 1))
        fi
    done
done
echo "outcome-diff done: $runs runs, $crashes crash/hang, $lenient lenient, $stricter stricter -> $OUT/"
