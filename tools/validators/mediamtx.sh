#!/usr/bin/env bash
# start mediamtx on the host network: WHIP publish and WHEP read on :8889, ICE on udp 8189
set -euo pipefail
image="${CALLIOPE_MEDIAMTX_IMAGE:-docker.io/bluenviron/mediamtx:latest}"
runtime="${CALLIOPE_CONTAINER_RUNTIME:-podman}"
"$runtime" rm -f calliope-mediamtx > /dev/null 2>&1 || true
exec "$runtime" run -d --rm --name calliope-mediamtx --network=host "$image"
