#!/usr/bin/env bash
# MP4Box from the gpac/ubuntu image, the last argument is the file to check
set -euo pipefail
image="${CALLIOPE_GPAC_IMAGE:-docker.io/gpac/ubuntu:latest}"
runtime="${CALLIOPE_CONTAINER_RUNTIME:-podman}"
file="$(realpath "${!#}")"
dir="$(dirname "$file")"
exec "$runtime" run --rm --network=none --security-opt label=disable \
    -v "$dir:$dir:ro" "$image" MP4Box "${@:1:$#-1}" "$file"
