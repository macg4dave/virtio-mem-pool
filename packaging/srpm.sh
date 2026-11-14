#!/usr/bin/env bash
# Build the source RPM that COPR builds the host and guest RPMs from
# (roadmap O2.7), from the commit checked out:
#
#   packaging/srpm.sh VERSION BUILD_ID OUTPUT_DIRECTORY [--rebuild]
#
# --rebuild also builds the binary RPMs from it (without checking build
# dependencies), so CI can compare them with the nfpm packages.
set -euo pipefail
if (( $# < 3 || $# > 4 )) || { (( $# == 4 )) && [[ $4 != --rebuild ]]; }; then
  echo 'usage: packaging/srpm.sh VERSION BUILD_ID OUTPUT_DIRECTORY [--rebuild]' >&2
  exit 1
fi
version=$1 build_id=$2 rebuild=${4:-}
[[ $version =~ ^[0-9A-Za-z.~^+]+$ ]] || { echo "not an RPM version: $version" >&2; exit 1; }
[[ $build_id =~ ^[0-9A-Za-z._-]+$ ]] || { echo "not a build identity: $build_id" >&2; exit 1; }
mkdir -p "$3"
output=$(cd -- "$3" && pwd)
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
top=$(mktemp -d)
trap 'rm -rf "$top"' EXIT
mkdir -p "$top/SOURCES" "$top/SPECS"

git -C "$repo" archive --format=tar.gz --prefix="rampire-$version/" \
  -o "$top/SOURCES/rampire-$version.tar.gz" HEAD
sed -e "s/@VERSION@/$version/" -e "s/@BUILD_ID@/$build_id/" \
  "$repo/packaging/rampire.spec.in" > "$top/SPECS/rampire.spec"
! grep -q '@[A-Z_]*@' "$top/SPECS/rampire.spec"

rpmbuild --define "_topdir $top" -bs "$top/SPECS/rampire.spec"
cp "$top"/SRPMS/*.src.rpm "$output/"
if [[ $rebuild == --rebuild ]]; then
  rpmbuild --define "_topdir $top" --nodeps --rebuild "$top"/SRPMS/*.src.rpm
  find "$top/RPMS" -name '*.rpm' -exec cp {} "$output/" \;
fi
ls -l "$output"
