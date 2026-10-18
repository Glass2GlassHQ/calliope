#!/usr/bin/env bash
# DASH-IF conformance tool from the image build-dashif.sh builds, the last argument is the manifest
set -euo pipefail
image="${CALLIOPE_DASHIF_IMAGE:-localhost/calliope-dashif:latest}"
runtime="${CALLIOPE_CONTAINER_RUNTIME:-podman}"
file="$(realpath "${!#}")"
dir="$(dirname "$file")"
# the network stays up: the xsd check fetches xml.xsd from www.w3.org
exec "$runtime" run --rm --security-opt label=disable -e QUEUE_CONNECTION=sync \
    -v "$dir:$dir:ro" "$image" php artisan app:analyze-manifest "$file" "IOP MPD" "IOP Segments"
