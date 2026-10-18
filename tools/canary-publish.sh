#!/usr/bin/env bash
# copies the canary status page to an s3 url or a local directory
set -euo pipefail

status_dir=${1:?usage: canary-publish.sh <status-dir> <s3://bucket/prefix | directory>}
destination=${2:?usage: canary-publish.sh <status-dir> <s3://bucket/prefix | directory>}
# the page changes every sample, so caches must not hold it longer than one publish
cache_control="max-age=60"

case "$destination" in
    s3://*)
        aws s3 sync --delete --cache-control "$cache_control" "$status_dir" "$destination"
        ;;
    *)
        mkdir -p "$destination"
        cp -a "$status_dir"/. "$destination"/
        ;;
esac
