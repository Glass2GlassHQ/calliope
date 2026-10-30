#!/usr/bin/env bash
# installs the deploy files the canary image carries and restarts the canary units, `pull` does it only for a new image
set -euo pipefail

command=${1:?usage: canary-update.sh <pull | install>}
image=$(sed -n 's/^Image=//p' "$HOME/.config/containers/systemd/g2g-canary.container")
install_dir=$HOME/g2g-canary
quadlet_dir=$HOME/.config/containers/systemd
unit_dir=$HOME/.config/systemd/user
installed_image_file=$install_dir/installed-image
staging=$install_dir/staging

image_id() {
    podman image inspect --format '{{.Id}}' "$image"
}

install_from_image() {
    rm -rf "$staging"
    mkdir -p "$staging" "$install_dir/data" "$quadlet_dir" "$unit_dir"
    container=$(podman create "$image")
    trap 'podman rm "$container" >/dev/null' EXIT
    podman cp "$container:/usr/share/g2g-canary/deploy/." "$staging"
    cp "$staging"/{mediamtx.yml,feeder.sh,canary.toml,canary-publish.sh,canary-alert.sh} "$install_dir/"
    # bash reads this script while it runs, so the new copy replaces it by rename
    cp "$staging/canary-update.sh" "$install_dir/canary-update.sh.new"
    mv "$install_dir/canary-update.sh.new" "$install_dir/canary-update.sh"
    cp "$staging"/*.container "$quadlet_dir/"
    cp "$staging"/canary-{publish,update}.{service,timer} "$unit_dir/"
    rm -rf "$staging"
    systemctl --user daemon-reload
    # the canary stops first, or it logs its readers' servers going down as incidents
    systemctl --user stop g2g-canary.service
    systemctl --user restart g2g-canary-mediamtx.service g2g-canary-feeder.service g2g-canary-faulted.service \
        g2g-canary-livesim2.service g2g-canary-peers.service
    systemctl --user start g2g-canary.service
    systemctl --user enable --now canary-update.timer
    if [ -f "$install_dir/publish.env" ]; then systemctl --user enable --now canary-publish.timer; fi
    image_id > "$installed_image_file"
}

case $command in
    pull)
        podman pull -q "$image" >/dev/null
        if [ "$(image_id)" != "$(cat "$installed_image_file")" ]; then install_from_image; fi
        ;;
    install) install_from_image ;;
    *) echo "usage: canary-update.sh <pull | install>" >&2; exit 1 ;;
esac
