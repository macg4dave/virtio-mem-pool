#!/usr/bin/env bash
# Create a copy of a Linux VM with a virtio-mem device, for multi-VM tests.
#
# The copy is provisioned fresh rather than copied from the template's disk:
# a qcow2 overlay on a pristine cloud image plus a cloud-init seed made from
# the template's user-data, with only the hostname changed. It therefore gets
# its own SSH host keys and machine-id. The domain XML is the template's
# (devices, virtio-mem device, control channel), with a new name, UUID, MAC,
# disk and seed, and optionally a different boot RAM.
#
# Usage: clone-vm.sh [options] TEMPLATE_VM NEW_VM HOSTNAME BASE_IMAGE USER_DATA
#   BASE_IMAGE  pristine qcow2 cloud image; it becomes the read-only backing
#               file of every copy, so never attach it to a VM itself
#   USER_DATA   the template's cloud-init user-data; a top-level "hostname:"
#               line is replaced with HOSTNAME
# Options:
#   --images DIR        where the copy's disk and seed go (default: the
#                       directory of the template's first disk)
#   --boot-mib N        fixed boot RAM in MiB (default: the template's)
#   --bootstrap-mib N   device memory plugged at first boot, while cloud-init
#                       installs packages before the guest service exists
#                       (default 1024); set it to 0 once the service runs
#   --disk-gib N        overlay size (default 15)
#   --mac MAC           (default: random, locally administered 52:54:00:...)
#   --apply             create, define and start the copy; without it, only
#                       print what would be done
# Evidence (the domain XML and what was done) goes to .artifacts/clones/.
set -euo pipefail

usage() {
  sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//' >&2
  exit 1
}

images='' boot_mib='' bootstrap_mib=1024 disk_gib=15 mac='' apply=false
positional=()
while (( $# )); do
  case $1 in
    --images) images=$2; shift 2 ;;
    --boot-mib) boot_mib=$2; shift 2 ;;
    --bootstrap-mib) bootstrap_mib=$2; shift 2 ;;
    --disk-gib) disk_gib=$2; shift 2 ;;
    --mac) mac=$2; shift 2 ;;
    --apply) apply=true; shift ;;
    -h|--help) usage ;;
    -*) echo "unknown option: $1" >&2; usage ;;
    *) positional+=("$1"); shift ;;
  esac
done
(( ${#positional[@]} == 5 )) || usage
template=${positional[0]} name=${positional[1]} host=${positional[2]}
base=$(realpath -- "${positional[3]}") user_data=${positional[4]}

[[ $name =~ ^[a-z0-9][a-z0-9_-]*$ ]] || { echo "VM names use a-z, 0-9, - and _" >&2; exit 1; }
[[ $host =~ ^[a-z0-9]([a-z0-9-]*[a-z0-9])?$ ]] || { echo "invalid hostname: $host" >&2; exit 1; }
for number in "$boot_mib" "$bootstrap_mib" "$disk_gib"; do
  [[ -z $number || $number =~ ^[0-9]+$ ]] || { echo "sizes are whole numbers: $number" >&2; exit 1; }
done
[[ -f $base && -f $user_data ]] || { echo "BASE_IMAGE and USER_DATA must exist" >&2; exit 1; }
[[ -z $mac || $mac =~ ^([0-9a-f]{2}:){5}[0-9a-f]{2}$ ]] || { echo "invalid MAC: $mac" >&2; exit 1; }

virsh=(virsh -c qemu:///system)
if "${virsh[@]}" dominfo "$name" >/dev/null 2>&1; then
  echo "a domain named $name already exists" >&2
  exit 1
fi
template_xml=$("${virsh[@]}" dumpxml --inactive "$template")
device_kib=$(sed -n "/<memory model='virtio-mem'>/,/<\/memory>/s/.*<size unit='KiB'>\([0-9]*\)<.*/\1/p" <<< "$template_xml")
template_boot_kib=$(sed -n "s/.*<cell id='0'[^>]* memory='\([0-9]*\)' unit='KiB'.*/\1/p" <<< "$template_xml")
[[ $device_kib =~ ^[0-9]+$ && $template_boot_kib =~ ^[0-9]+$ ]] || {
  echo "$template needs one virtio-mem device and one NUMA cell, both in KiB" >&2
  exit 1
}
boot_kib=$(( ${boot_mib:-$(( template_boot_kib / 1024 ))} * 1024 ))
total_mib=$(( (boot_kib + device_kib) / 1024 ))
template_disk=$("${virsh[@]}" domblklist "$template" --details | awk '$2 == "disk" { print $4; exit }')
images=${images:-$(dirname -- "$template_disk")}
disk=$images/$name.qcow2 seed=$images/$name-seed.iso
mac=${mac:-$(printf '52:54:00:%02x:%02x:%02x' $((RANDOM % 256)) $((RANDOM % 256)) $((RANDOM % 256)))}
[[ ! -e $disk && ! -e $seed ]] || { echo "$disk or $seed already exists" >&2; exit 1; }

cat <<EOF
copy:        $template -> $name (hostname $host, MAC $mac)
disk:        $disk (${disk_gib} GiB overlay on $base)
seed:        $seed
boot RAM:    $(( boot_kib / 1024 )) MiB fixed + $(( device_kib / 1024 )) MiB virtio-mem device
first boot:  $bootstrap_mib MiB requested on the device
EOF
if ! $apply; then
  echo "dry run: pass --apply to create, define and start it"
  exit 0
fi

evidence_dir=$(cd -- "$(dirname -- "$0")/../.." && pwd)/.artifacts/clones
mkdir -p "$evidence_dir"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
sed "s/^hostname:.*/hostname: $host/" "$user_data" > "$work/user-data"
printf 'instance-id: %s-%s\nlocal-hostname: %s\n' "$name" "$(date -u +%Y%m%dT%H%M%SZ)" "$host" > "$work/meta-data"
xorriso -as mkisofs -quiet -output "$work/seed.iso" -volid cidata -joliet -rock \
  "$work/user-data" "$work/meta-data"
sudo -n qemu-img create -q -f qcow2 -F qcow2 -b "$base" "$disk" "${disk_gib}G"
sudo -n install -m 0644 "$work/seed.iso" "$seed"

virt-xml --edit --metadata "name=$name,uuid=$(cat /proc/sys/kernel/random/uuid)" <<< "$template_xml" \
  | virt-xml --edit target=vda --disk "path=$disk" \
  | virt-xml --edit target=hda --disk "path=$seed" \
  | virt-xml --edit --network "mac=$mac" \
  | virt-xml --edit --cpu "cell0.memory=$boot_kib" \
  | virt-xml --edit --memory "memory=$total_mib,currentMemory=$total_mib" \
  | virt-xml --edit --memdev "target.requested=$(( bootstrap_mib * 1024 ))" \
  > "$work/domain.xml"
"${virsh[@]}" define --validate "$work/domain.xml"
cp "$work/domain.xml" "$evidence_dir/$name.xml"
"${virsh[@]}" start "$name"
printf '{"vm":"%s","template":"%s","hostname":"%s","mac":"%s","disk":"%s","base":"%s","seed":"%s","boot_mib":%d,"device_mib":%d,"bootstrap_mib":%d,"created":"%s"}\n' \
  "$name" "$template" "$host" "$mac" "$disk" "$base" "$seed" $(( boot_kib / 1024 )) \
  $(( device_kib / 1024 )) "$bootstrap_mib" "$(date -u +%FT%TZ)" > "$evidence_dir/$name.json"
echo "started $name; evidence in $evidence_dir/$name.json"
