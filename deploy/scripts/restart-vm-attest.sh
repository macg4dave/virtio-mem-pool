#!/usr/bin/env bash
# Cold-restart one VM so its persistent definition (for example a new
# virtio-mem block size) takes effect, attest the new live geometry, and
# install the host controller with that attestation.
# Usage: restart-vm-attest.sh INSTANCE VM ALIAS REVIEW
# INSTANCE names the systemd controller; REVIEW is the VM's reviewed
# compatibility review (e.g. .artifacts/install/win11_gpu.review.json).
set -euo pipefail
config_dir=$(cd -- "$(dirname -- "$0")" && pwd)
cd "$config_dir/../.."
if (( $# != 4 )); then
  echo 'usage: restart-vm-attest.sh INSTANCE VM ALIAS REVIEW' >&2
  exit 1
fi
instance=$1 vm=$2 alias=$3 review=$4
if [[ ! -f $review ]]; then
  echo "review file not found: $review" >&2
  exit 1
fi
virsh=(virsh -c qemu:///system)
unit=virtio-mem-host@${instance}.service
out_dir=.artifacts/block-size
attestation=$out_dir/${vm}.attestation.$(date +%Y%m%dT%H%M%S).json
mkdir -p "$out_dir"

# The controller must not observe the VM going away.
sudo systemctl stop "$unit"

# A guest reboot keeps the running QEMU; only shutdown + start applies the
# persistent definition.
if [[ $("${virsh[@]}" domstate "$vm") != 'shut off' ]]; then
  "${virsh[@]}" shutdown "$vm"
  for _ in $(seq 300); do
    [[ $("${virsh[@]}" domstate "$vm") == 'shut off' ]] && break
    sleep 2
  done
  if [[ $("${virsh[@]}" domstate "$vm") != 'shut off' ]]; then
    echo "$vm did not shut off within 10 minutes" >&2
    exit 1
  fi
fi
"${virsh[@]}" start "$vm"
"${virsh[@]}" dumpxml "$vm" | grep -E '<block|<requested|<current'

# Attest only once Windows is up: wait for the guest agent.
echo "waiting for the $vm guest agent..."
for _ in $(seq 300); do
  "${virsh[@]}" qemu-agent-command "$vm" '{"execute":"guest-ping"}' >/dev/null 2>&1 && break
  sleep 2
done
if ! "${virsh[@]}" qemu-agent-command "$vm" '{"execute":"guest-ping"}' >/dev/null 2>&1; then
  echo "$vm guest agent did not answer within 10 minutes" >&2
  exit 1
fi

cargo xtask attestation "$vm" "$alias" "$review" \
  --output "$attestation" --command-timeout-seconds 60
echo "attestation: $attestation"
"$config_dir/install-host.sh" "$instance" "$vm=$attestation"
