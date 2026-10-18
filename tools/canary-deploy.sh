#!/usr/bin/env bash
# ships the canary image and deploy files to <ssh-host> and restarts its user units
set -euo pipefail

host=${1:?usage: canary-deploy.sh <ssh-host>}
image=${CANARY_IMAGE:-localhost/g2g-canary:latest}
calliope_dir=$(cd "$(dirname "$0")/.." && pwd)
deploy_dir=$calliope_dir/deploy/canary

podman save "$image" | gzip | ssh "$host" 'gunzip | podman load'
ssh "$host" 'mkdir -p g2g-canary/data .config/systemd/user'
scp "$deploy_dir"/{compose.yml,mediamtx.yml,feeder.sh,canary.toml} \
    "$calliope_dir/tools/canary-publish.sh" "$host":g2g-canary/
scp "$deploy_dir"/{g2g-canary.service,canary-publish.service,canary-publish.timer} \
    "$host":.config/systemd/user/
ssh "$host" 'systemctl --user daemon-reload \
    && systemctl --user enable g2g-canary.service \
    && systemctl --user restart g2g-canary.service \
    && if [ -f g2g-canary/publish.env ]; then systemctl --user enable --now canary-publish.timer; fi \
    && systemctl --user --no-pager status g2g-canary.service | head -5'
