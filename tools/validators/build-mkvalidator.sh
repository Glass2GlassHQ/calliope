#!/usr/bin/env bash
# mkvalidator has no distro package or maintained image: build the 0.6.0 tag.
# release build, the debug one aborts on an assertion at exit.
set -euo pipefail
PREFIX="${PREFIX:-$HOME/.local/bin}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
git clone -q --depth 1 --branch mkvalidator-0.6.0 \
    https://github.com/Matroska-Org/foundation-source "$WORK/src"
cmake -S "$WORK/src" -B "$WORK/build" -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_POLICY_VERSION_MINIMUM=3.5 > /dev/null
cmake --build "$WORK/build" --target mkvalidator -j2 > /dev/null
install -m 755 "$WORK/build/mkvalidator/mkvalidator" "$PREFIX/mkvalidator"
echo "installed $PREFIX/mkvalidator"
