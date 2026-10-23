#!/bin/bash
# Finish a release directory once both architectures are built into it:
# add install.sh, the Arch PKGBUILD, and SHA256SUMS over everything.
#
#   packaging/assemble.sh DIR VERSION
set -euo pipefail
[ $# = 2 ] || { sed -n '2,6p' "$0"; exit 2; }
dir=$1 version=$2
here=$(cd "$(dirname "$0")" && pwd)
cp "$here/install.sh" "$dir/"
cd "$dir"
sha256sum orion-*-linux.tar.gz > SHA256SUMS
"$here/pkgbuild.sh" "$version" SHA256SUMS > PKGBUILD
sha256sum orion-*-linux.tar.gz *.deb *.rpm install.sh PKGBUILD > SHA256SUMS
cat SHA256SUMS
