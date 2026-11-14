#!/usr/bin/env bash
# Put a running Linux copy of a managed VM (see clone-vm.sh) under the host
# controller: install the guest service, give the copy the template's
# settings and review under its own name, attest it, make its saved device
# request zero so later boots start on boot RAM alone, and install the
# controller with every VM's attestation.
#
# Usage: add-linux-vm.sh [--apply] CONFIG_DIR INSTANCE TEMPLATE_VM NEW_VM SSH_DESTINATION
#   CONFIG_DIR       the site configuration install-host.sh takes; it must hold
#                    TEMPLATE_VM_settings.toml, and in its attestations/ folder
#                    TEMPLATE_VM.review.json and an attestation for every VM in
#                    host_list.toml
#   SSH_DESTINATION  the copy, for an account with passwordless sudo
# Without --apply it only checks the inputs and prints the plan.
set -euo pipefail

apply=false
if [[ ${1:-} == --apply ]]; then
  apply=true
  shift
fi
if (( $# != 5 )); then
  sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//' >&2
  exit 1
fi
config_dir=$(cd -- "$1" && pwd)
instance=$2 template=$3 name=$4 destination=$5
scripts_dir=$(cd -- "$(dirname -- "$0")" && pwd)
repo=$(cd -- "$scripts_dir/../.." && pwd)
virsh=(virsh -c qemu:///system)
# A copy made moments ago has a host key nobody has seen: trust it on first
# use, as anyone logging in to a new VM does, and check it from then on.
ssh_guest=(ssh -o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new -- "$destination")

[[ $name =~ ^[a-z0-9][a-z0-9_-]*$ ]] || { echo "invalid VM name: $name" >&2; exit 1; }
evidence=$config_dir/attestations
for file in host_list.toml "${template}_settings.toml" "attestations/$template.review.json"; do
  [[ -f $config_dir/$file ]] || { echo "missing $config_dir/$file" >&2; exit 1; }
done
"${virsh[@]}" dominfo "$name" >/dev/null
alias=$("${virsh[@]}" dumpxml "$name" \
  | sed -n "/<memory model='virtio-mem'>/,/<\/memory>/s/.*<alias name='\([^']*\)'.*/\1/p")
[[ -n $alias ]] || { echo "$name has no virtio-mem device alias" >&2; exit 1; }
priority=$(awk -v vm="$template" '
  /^\[\[vm\]\]/ { found = 0 }
  $1 == "vm_name" && $3 == "\"" vm "\"" { found = 1 }
  found && $1 == "priority" { print $3; exit }' "$config_dir/host_list.toml")
[[ $priority =~ ^[0-9]+$ ]] || { echo "no priority for $template in host_list.toml" >&2; exit 1; }
listed=false
grep -q "^vm_name = \"$name\"$" "$config_dir/host_list.toml" && listed=true

echo "add $name (alias $alias, priority $priority) to controller $instance via $destination"
$listed && echo "  already in host_list.toml"
if ! $apply; then
  echo "dry run: pass --apply first to change anything"
  exit 0
fi

# cloud-init reboots once when it finishes: wait until it reports done (or
# disabled, for a guest that turns it off after provisioning) on a
# boot that then stays up.
echo "waiting for $destination to finish provisioning..."
for _ in $(seq 120); do
  boot=$("${ssh_guest[@]}" 'cloud-init status 2>/dev/null | grep -Eq "status: (done|disabled)" && cat /proc/sys/kernel/random/boot_id' 2>/dev/null || true)
  if [[ -n $boot ]]; then
    sleep 20
    [[ $("${ssh_guest[@]}" 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null || true) == "$boot" ]] && break
  fi
  sleep 5
  boot=''
done
[[ -n $boot ]] || { echo "$destination did not finish provisioning in time" >&2; exit 1; }

cd "$repo"
mkdir -p ".artifacts/clones"
cargo xtask linux deploy "$destination" --output ".artifacts/clones/$name.guest-deploy.json" --apply

kernel=$("${ssh_guest[@]}" uname -r)
sed "s/^vm_name = .*/vm_name = \"$name\"/" "$config_dir/${template}_settings.toml" \
  > "$config_dir/${name}_settings.toml"
sed "s/\"guest_driver_version\": \"[^\"]*\"/\"guest_driver_version\": \"linux $kernel\"/" \
  "$evidence/$template.review.json" > "$evidence/$name.review.json"
if ! $listed; then
  printf '\n[[vm]]\nvm_name = "%s"\nalias = "%s"\npriority = %s\n' \
    "$name" "$alias" "$priority" >> "$config_dir/host_list.toml"
fi
"${virsh[@]}" update-memory-device "$name" --alias "$alias" --requested-size 0 --config
cargo xtask attestation "$name" "$alias" "$evidence/$name.review.json" \
  --output "$evidence/$name.attestation.json" --command-timeout-seconds 60

# install-host.sh takes every VM's attestation from $evidence.
"$scripts_dir/install-host.sh" "$config_dir" "$instance"
