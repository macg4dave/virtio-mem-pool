#!/usr/bin/env bash
# Build RAMpire's release artefacts from binaries already built:
#   target/release/{rampire,rampire-guest}                      for packages
#   target/x86_64-unknown-linux-musl/release/{rampire,rampire-guest}
#                                                               for tarballs
#
#   packaging/build.sh VERSION OUTPUT_DIRECTORY
#
# Writes the host and guest packages as RPM and .deb (nfpm), a static
# tarball of each when the musl binaries exist, and SHA256SUMS.
set -euo pipefail
if (( $# != 2 )); then
  echo 'usage: packaging/build.sh VERSION OUTPUT_DIRECTORY' >&2
  exit 1
fi
version=$1
mkdir -p "$2"
output=$(cd -- "$2" && pwd)
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
command -v nfpm >/dev/null || { echo 'nfpm is required: https://nfpm.goreleaser.com' >&2; exit 1; }

# Packages install the guest binary in /usr/libexec, not /usr/local. The
# nfpm definitions name their files relative to the repository root.
cd "$repo"
mkdir -p target/packaging
sed 's#/usr/local/libexec/rampire-guest#/usr/libexec/rampire-guest#' \
  guest-linux/systemd/rampire-guest.service > target/packaging/rampire-guest.service
grep -q '^ExecStart=/usr/libexec/rampire-guest ' target/packaging/rampire-guest.service
cp target/packaging/rampire-guest.service "$stage/rampire-guest.service"

export VERSION=$version ARCH=amd64
for package in rampire rampire-guest; do
  for packager in rpm deb; do
    nfpm package --config "packaging/$package.nfpm.yaml" \
      --packager "$packager" --target "$output/"
  done
done

musl=$repo/target/x86_64-unknown-linux-musl/release
if [[ -x $musl/rampire && -x $musl/rampire-guest ]]; then
  host=$stage/rampire-$version-x86_64-linux
  mkdir -p "$host/systemd" "$host/examples/vms.d" "$host/examples/reviews"
  install -m 0755 "$musl/rampire" "$host/rampire"
  install -m 0644 "$repo/host/systemd/rampire.service" "$repo/host/systemd/50-rampire.rules" \
    "$repo/packaging/rampire.sysusers" "$repo/packaging/debian-socket-group.conf" "$host/systemd/"
  install -m 0644 "$repo/deploy/examples/global.toml" "$host/examples/"
  install -m 0644 "$repo/deploy/examples/vms.d/linux-vm.toml" "$host/examples/vms.d/"
  install -m 0644 "$repo/deploy/examples/reviews/linux-standard.toml" "$host/examples/reviews/"
  install -m 0644 "$repo/packaging/INSTALL.md" "$repo/LICENSE" "$host/"
  tar -C "$stage" -czf "$output/rampire-$version-x86_64-linux.tar.gz" "${host##*/}"

  guest=$stage/rampire-guest-$version-x86_64-linux
  mkdir -p "$guest"
  install -m 0755 "$musl/rampire-guest" "$guest/rampire-guest"
  install -m 0644 "$stage/rampire-guest.service" "$repo/packaging/INSTALL.md" "$repo/LICENSE" "$guest/"
  install -m 0755 "$repo/tools/linux/watch-ram.sh" "$guest/rampire-watch-ram"
  tar -C "$stage" -czf "$output/rampire-guest-$version-x86_64-linux.tar.gz" "${guest##*/}"
fi

# With RAMPIRE_SIGNING_KEY_ID (a key in the GnuPG keyring), every RPM in the
# output, the source RPM included, is signed before the checksums. The
# .deb packages are covered by the apt repository's signature.
if [[ -n ${RAMPIRE_SIGNING_KEY_ID:-} ]]; then
  rpmsign --define "_gpg_name $RAMPIRE_SIGNING_KEY_ID" --addsign "$output"/*.rpm
fi

(cd "$output" && rm -f SHA256SUMS && sha256sum -- * > "$stage/SHA256SUMS" && mv "$stage/SHA256SUMS" SHA256SUMS)
ls -l "$output"
