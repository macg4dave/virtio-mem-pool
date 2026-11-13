#!/usr/bin/env bash
# Recover a latched controller: stop it, clear the latch with the installed
# attestations (clear-latch first settles a stuck shrink at what the guest
# actually kept), start it again. Prompts for sudo. Install the current host
# build and attestation first (install-host.sh).
#
#   deploy/scripts/recover-latch.sh INSTANCE "reason recorded in the checkpoint"
#
# VIRTIO_MEM_HOST_BIN, VIRTIO_MEM_HOST_CONFIG and VIRTIO_MEM_CONNECT override
# the installed binary, host list and libvirt URI.
set -euo pipefail
if (( $# != 2 )); then
  echo 'usage: recover-latch.sh INSTANCE REASON' >&2
  exit 1
fi
instance=$1 reason=$2
if [[ ! $instance =~ ^[[:alnum:]][[:alnum:]_.-]*$ ]]; then
  echo 'invalid controller instance name' >&2
  exit 1
fi
unit=virtio-mem-host@$instance.service
config=${VIRTIO_MEM_HOST_CONFIG:-/etc/virtio-mem-host/host_list.toml}
binary=${VIRTIO_MEM_HOST_BIN:-/usr/local/libexec/virtio-mem-host}
connect=${VIRTIO_MEM_CONNECT:-qemu:///system}
host=(sudo -u virtio-mem-host "$binary")

sudo systemctl stop "$unit"
# Start it again however recovery ends; a still-latched controller only
# observes.
trap 'sudo systemctl start "$unit"; sleep 5; systemctl status "$unit" --no-pager | head -8' EXIT
"${host[@]}" clear-latch "$reason" --config "$config" --connect "$connect" --apply
