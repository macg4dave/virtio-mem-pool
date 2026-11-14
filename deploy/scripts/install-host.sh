#!/usr/bin/env bash
# Build the host controller from this checkout and install it with a site's
# configuration and explicit, current attestations.
# Usage: install-host.sh CONFIG_DIR INSTANCE [VM=ATTESTATION ...]
# CONFIG_DIR holds global.toml and vms.d/<vm_name>.toml for each VM (see
# deploy/examples), installed exactly as written. Each VM's attestation is CONFIG_DIR/attestations/<vm_name>.attestation.json
# unless VM=ATTESTATION names another file. INSTANCE names the systemd
# controller.
set -euo pipefail
if (( $# < 2 )); then
  echo 'usage: install-host.sh CONFIG_DIR INSTANCE [VM=ATTESTATION ...]' >&2
  exit 1
fi
config_dir=$(cd -- "$1" && pwd)
instance=$2
shift 2
if [[ ! -d $config_dir/vms.d ]]; then
  echo "no vms.d folder in $config_dir" >&2
  exit 1
fi
if [[ ! $instance =~ ^[[:alnum:]][[:alnum:]_.-]*$ ]]; then
  echo 'invalid controller instance name' >&2
  exit 1
fi
attestations=()
if [[ -d $config_dir/attestations ]]; then
  attestations+=(--attestation-dir "$config_dir/attestations")
fi
for binding in "$@"; do
  if [[ $binding != *=* || -z ${binding%%=*} || ! -f ${binding#*=} ]]; then
    echo "expected VM=ATTESTATION with an existing file: $binding" >&2
    exit 1
  fi
  attestations+=(--attestation "${binding%%=*}=$(cd -- "$(dirname -- "${binding#*=}")" && pwd)/$(basename -- "${binding#*=}")")
done
cd "$(dirname -- "$0")/../.."
unit=rampire@${instance}.service
if pgrep -f 'target/release/rampire' >/dev/null; then
  echo 'a foreground rampire is running; stop it first' >&2
  exit 1
fi
mkdir -p .artifacts/deploy
# rampire --version reports this build: the commit, and -dirty for
# uncommitted changes.
RAMPIRE_BUILD_ID=$(git rev-parse --short=12 HEAD)$(git diff --quiet HEAD -- || echo -dirty)
export RAMPIRE_BUILD_ID
cargo build --release --locked -p rampire -p xtask
# host-deploy validates exclusive ownership and requires this unit inactive;
# a unit left `failed` (for example by a polkit outage) is not inactive.
sudo systemctl stop "$unit"
sudo systemctl reset-failed "$unit" 2>/dev/null || true
target/release/xtask host-deploy "$instance" \
  --config "$config_dir" \
  "${attestations[@]}" \
  --output ".artifacts/deploy/host-deploy-$(date +%Y%m%dT%H%M%S).json" \
  --command-timeout-seconds 60 --apply --elevate
# The unit runs the applied copy, so apply the installed draft before the
# unit starts. It checks every VM again and changes nothing if one fails.
sudo /usr/local/bin/rampire apply --yes
sudo systemctl enable --now "$unit"
systemctl status "$unit" --no-pager
