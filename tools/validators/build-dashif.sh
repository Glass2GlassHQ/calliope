#!/usr/bin/env bash
# DASH-IF conformance tool image, upstream publishes none: build its Dockerfile at a pinned commit.
set -euo pipefail
commit="${DASHIF_COMMIT:-e311ec6e12b21394355fd1749101590400e31cbb}"
image="${CALLIOPE_DASHIF_IMAGE:-localhost/calliope-dashif:latest}"
runtime="${CALLIOPE_CONTAINER_RUNTIME:-podman}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
git -C "$WORK" init -q
git -C "$WORK" fetch -q --depth 1 https://github.com/Dash-Industry-Forum/DASH-IF-Conformance "$commit"
git -C "$WORK" checkout -q FETCH_HEAD
# podman needs fully qualified image names
sed -i 's|^FROM dunglas/frankenphp AS base|FROM docker.io/dunglas/frankenphp AS base\nRUN sed -i "s\|http://\|https://\|g" /etc/apt/sources.list.d/debian.sources|' "$WORK/Dockerfile"
sed -i 's|COPY --from=composer:latest|COPY --from=docker.io/library/composer:latest|; s|make -j$|make -j2|' "$WORK/Dockerfile"
cp "$WORK/env_production.example" "$WORK/.env"
"$runtime" build --jobs 1 -t "$image" -f "$WORK/Dockerfile" "$WORK"
echo "built $image"
