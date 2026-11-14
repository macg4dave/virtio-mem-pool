#!/usr/bin/env bash
# Build the signed apt repository that GitHub Pages serves (roadmap O2.7)
# from a release's .deb packages:
#
#   packaging/apt-repo.sh PACKAGE_DIRECTORY OUTPUT_DIRECTORY
#
# The repository holds the release being built: one suite, stable, one
# component, main. Its Release file is signed with the only secret key in
# the GnuPG keyring, and the public key is published beside it as
# rampire.asc. Needs apt-ftparchive (apt-utils) and gpg.
set -euo pipefail
if (( $# != 2 )); then
  echo 'usage: packaging/apt-repo.sh PACKAGE_DIRECTORY OUTPUT_DIRECTORY' >&2
  exit 1
fi
packages=$(cd -- "$1" && pwd)
mkdir -p "$2"
output=$(cd -- "$2" && pwd)
repo=$output/apt
fingerprints=$(gpg --batch --with-colons --list-secret-keys | awk -F: '$1 == "fpr" { print $10 }')
(( $(wc -l <<< "$fingerprints") == 1 )) && [[ -n $fingerprints ]] \
  || { echo 'the keyring needs exactly one secret key' >&2; exit 1; }
key=${fingerprints%%$'\n'*}
compgen -G "$packages/*.deb" >/dev/null || { echo "no .deb in $packages" >&2; exit 1; }

rm -rf "$repo"
mkdir -p "$repo/pool/main" "$repo/dists/stable/main/binary-amd64"
cp "$packages"/*.deb "$repo/pool/main/"
cd "$repo"
apt-ftparchive packages pool > dists/stable/main/binary-amd64/Packages
gzip -9 -k dists/stable/main/binary-amd64/Packages
apt-ftparchive \
  -o APT::FTPArchive::Release::Origin=RAMpire \
  -o APT::FTPArchive::Release::Label=RAMpire \
  -o APT::FTPArchive::Release::Suite=stable \
  -o APT::FTPArchive::Release::Codename=stable \
  -o APT::FTPArchive::Release::Architectures=amd64 \
  -o APT::FTPArchive::Release::Components=main \
  release dists/stable > "$output/Release"
mv "$output/Release" dists/stable/Release
gpg --batch --yes --local-user "$key" --clearsign -o dists/stable/InRelease dists/stable/Release
gpg --batch --yes --local-user "$key" --armor --detach-sign -o dists/stable/Release.gpg dists/stable/Release
gpg --batch --armor --export "$key" > rampire.asc
cat > rampire.sources <<'EOF'
Types: deb
URIs: https://macg4dave.github.io/RAMpire/apt
Suites: stable
Components: main
Signed-By: /etc/apt/keyrings/rampire.asc
EOF
find "$repo" -type f | sort
