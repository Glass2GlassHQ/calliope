#!/usr/bin/env bash
# tsp from an unpacked TSDuck tree (no root install), TSDUCK_ROOT overrides
set -euo pipefail
root="${TSDUCK_ROOT:-$HOME/opt/tsduck/usr}"
export LD_LIBRARY_PATH="$root/lib64${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export TSPLUGINS_PATH="$root/lib64/tsduck"
exec "$root/bin/tsp" "$@"
