#!/usr/bin/env bash
# builds localhost/g2g-canary from the committed HEAD of the g2g and calliope checkouts
set -euo pipefail

g2g_dir=${1:-$HOME/src/glass2glass}
image=${CANARY_IMAGE:-localhost/g2g-canary:latest}
calliope_dir=$(cd "$(dirname "$0")/.." && pwd)
context=$(mktemp -d)
trap 'rm -rf "$context"' EXIT

mkdir -p "$context/g2g" "$context/calliope"
git -C "$g2g_dir" archive HEAD | tar -x -C "$context/g2g"
git -C "$calliope_dir" archive HEAD | tar -x -C "$context/calliope"

podman build \
    --build-context g2g="$context/g2g" \
    --build-arg CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" \
    --build-arg G2G_COMMIT="$(git -C "$g2g_dir" rev-parse HEAD)" \
    -f "$context/calliope/deploy/canary/Containerfile" \
    -t "$image" \
    "$context/calliope"
