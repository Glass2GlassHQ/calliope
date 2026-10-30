#!/usr/bin/env bash
# puts the canary image on <ssh-host>, or on this machine with `local`, and installs the deploy files it carries
set -euo pipefail

host=${1:?usage: canary-deploy.sh <ssh-host | local>}
calliope_dir=$(cd "$(dirname "$0")/.." && pwd)
deploy_dir=$calliope_dir/deploy/canary
deployed_image=$(sed -n 's/^Image=//p' "$deploy_dir/g2g-canary.container")
install_dir=g2g-canary
quadlet_dir=.config/containers/systemd

on_host() {
    if [ "$host" = local ]; then (cd && bash -c "$1"); else ssh "$host" "$1"; fi
}

copy_to() {
    local destination=$1
    shift
    if [ "$host" = local ]; then cp "$@" "$HOME/$destination/"; else scp "$@" "$host:$destination/"; fi
}

# replaces the security group's ssh rule with this machine's current address
allow_ssh_from_here() {
    local group=$1 address rules stale
    address=$(curl -fsS https://checkip.amazonaws.com)/32
    rules=$(aws ec2 describe-security-group-rules --filters Name=group-id,Values="$group" \
        --query 'SecurityGroupRules[?!IsEgress && FromPort==`22`].[SecurityGroupRuleId,CidrIpv4]' \
        --output text)
    stale=$(awk -v keep="$address" '$2 != keep {print $1}' <<<"$rules")
    if [ -n "$stale" ]; then
        aws ec2 revoke-security-group-ingress --group-id "$group" \
            --security-group-rule-ids $stale >/dev/null
    fi
    if ! awk -v keep="$address" '$2 == keep {found = 1} END {exit !found}' <<<"$rules"; then
        aws ec2 authorize-security-group-ingress --group-id "$group" \
            --protocol tcp --port 22 --cidr "$address" >/dev/null
    fi
}

if [ -n "${CANARY_SECURITY_GROUP:-}" ]; then
    allow_ssh_from_here "$CANARY_SECURITY_GROUP"
fi
# a CANARY_IMAGE from this machine runs until canary-update.timer next pulls a different registry image
if [ -z "${CANARY_IMAGE:-}" ]; then
    on_host "podman pull $deployed_image"
elif [ "$host" = local ]; then
    podman tag "$CANARY_IMAGE" "$deployed_image"
else
    podman save "$CANARY_IMAGE" | gzip | ssh "$host" "gunzip | podman load && podman tag $CANARY_IMAGE $deployed_image"
fi
on_host "mkdir -p $install_dir $quadlet_dir"
copy_to "$install_dir" "$deploy_dir/canary-update.sh"
copy_to "$quadlet_dir" "$deploy_dir/g2g-canary.container"
on_host "$install_dir/canary-update.sh install \
    && systemctl --user --no-pager status g2g-canary.service | head -5"
