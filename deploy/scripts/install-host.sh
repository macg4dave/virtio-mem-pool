#!/usr/bin/env bash
# Install the sibling host list/settings with explicit, current attestations.
# Usage: install-host.sh INSTANCE VM=ATTESTATION [VM=ATTESTATION ...]
# INSTANCE names the systemd controller; each VM needs its own reviewed evidence.
set -euo pipefail
config_dir=$(cd -- "$(dirname -- "$0")" && pwd)
cd "$config_dir/../.."
if (( $# < 2 )); then
  echo 'usage: install-host.sh INSTANCE VM=ATTESTATION [VM=ATTESTATION ...]' >&2
  exit 1
fi
instance=$1
shift
if [[ ! $instance =~ ^[[:alnum:]][[:alnum:]_.-]*$ ]]; then
  echo 'invalid controller instance name' >&2
  exit 1
fi
attestations=()
for binding in "$@"; do
  if [[ $binding != *=* || -z ${binding%%=*} || ! -f ${binding#*=} ]]; then
    echo "expected VM=ATTESTATION with an existing file: $binding" >&2
    exit 1
  fi
  attestations+=(--attestation "$binding")
done
unit=virtio-mem-host@${instance}.service
if pgrep -f 'target/release/virtio-mem-host' >/dev/null; then
  echo 'a foreground virtio-mem-host is running; stop it first' >&2
  exit 1
fi
mkdir -p .artifacts/deploy
cargo build --release --locked -p virtio-mem-host -p virtio-mem-xtask
# host-deploy validates exclusive ownership and requires this unit inactive;
# a unit left `failed` (for example by a polkit outage) is not inactive.
sudo systemctl stop "$unit"
sudo systemctl reset-failed "$unit" 2>/dev/null || true
target/release/virtio-mem-xtask host-deploy "$instance" \
  --config "$config_dir/host_list.toml" \
  "${attestations[@]}" \
  --output ".artifacts/deploy/host-deploy-$(date +%Y%m%dT%H%M%S).json" \
  --command-timeout-seconds 60 --apply --elevate
sudo systemctl enable --now "$unit"
systemctl status "$unit" --no-pager
