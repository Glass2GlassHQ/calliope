#!/usr/bin/env bash
# installs the canary image, deploy files and user units on <ssh-host>, or on this machine with `local`
set -euo pipefail

host=${1:?usage: canary-deploy.sh <ssh-host | local>}
# the tag g2g-canary.container runs
deployed_image=localhost/g2g-canary:latest
image=${CANARY_IMAGE:-$deployed_image}
calliope_dir=$(cd "$(dirname "$0")/.." && pwd)
deploy_dir=$calliope_dir/deploy/canary
install_dir=g2g-canary
quadlet_dir=.config/containers/systemd
unit_dir=.config/systemd/user

on_host() {
    if [ "$host" = local ]; then (cd && bash -c "$1"); else ssh "$host" "$1"; fi
}

copy_to() {
    local destination=$1
    shift
    if [ "$host" = local ]; then cp "$@" "$HOME/$destination/"; else scp "$@" "$host:$destination/"; fi
}

if [ "$host" != local ]; then
    podman save "$image" | gzip | ssh "$host" "gunzip | podman load && podman tag $image $deployed_image"
fi
on_host "mkdir -p $install_dir/data $quadlet_dir $unit_dir"
copy_to "$install_dir" "$deploy_dir"/{mediamtx.yml,feeder.sh,canary.toml} "$calliope_dir/tools/canary-publish.sh"
copy_to "$quadlet_dir" "$deploy_dir"/*.container
copy_to "$unit_dir" "$deploy_dir"/canary-publish.{service,timer}
on_host "systemctl --user daemon-reload \
    && systemctl --user restart g2g-canary-mediamtx.service g2g-canary-feeder.service g2g-canary.service \
    && if [ -f $install_dir/publish.env ]; then systemctl --user enable --now canary-publish.timer; fi \
    && systemctl --user --no-pager status g2g-canary.service | head -5"
