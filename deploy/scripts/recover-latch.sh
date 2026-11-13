#!/usr/bin/env bash
# Recover a latched win11_gpu controller: stop it, clear the latch with the
# installed attestation (clear-latch first settles a stuck shrink at what the
# guest actually kept), start it again. Prompts for sudo. Install the current
# host build and attestation first (install-host.sh).
#
#   deploy/config_files/recover-latch.sh "reason recorded in the checkpoint"
set -euo pipefail
cd "$(dirname "$0")/../.."
reason=${1:?usage: recover-latch.sh REASON}
vm=win11_gpu
unit=virtio-mem-host@$vm.service
config=/etc/virtio-mem-host/host_list.toml
host=(sudo -u virtio-mem-host /usr/local/libexec/virtio-mem-host)
connect=qemu:///system

sudo systemctl stop "$unit"
# Start it again however recovery ends; a still-latched controller only
# observes.
trap 'sudo systemctl start "$unit"; sleep 5; systemctl status "$unit" --no-pager | head -8' EXIT
"${host[@]}" clear-latch "$reason" --config "$config" --connect "$connect" --apply
