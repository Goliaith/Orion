#!/bin/bash
# mkdeb.sh NAME VERSION OUTDIR [extra control lines...] -> OUTDIR/NAME_VERSION-1_all.deb
set -e
name=$1 ver=$2 out=$3; shift 3
d=$(mktemp -d); mkdir -p $d/DEBIAN $d/usr/share/$name; echo $ver > $d/usr/share/$name/version
{ echo "Package: $name"; echo "Version: $ver-1"; echo "Architecture: all"; echo "Maintainer: Orion <orion@example.org>"
  echo "Description: Orion test package"; for l in "$@"; do echo "$l"; done; } > $d/DEBIAN/control
mkdir -p $out; dpkg-deb -Zgzip --root-owner-group -b $d $out/${name}_$ver-1_all.deb >/dev/null; rm -rf "$d"
