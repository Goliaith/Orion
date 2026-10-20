#!/bin/bash
# mk.sh NAME VERSION OUTDIR [extra .PKGINFO lines...]
set -e
name=$1 ver=$2 out=$3; shift 3
d=$(mktemp -d); mkdir -p $d/usr/share/$name; echo $ver > $d/usr/share/$name/version
{ echo "pkgname = $name"; echo "pkgbase = $name"; echo "pkgver = $ver-1"; echo "pkgdesc = Orion test package"
  echo "url = https://example.org"; echo "builddate = $(date +%s)"; echo "packager = Orion"; echo "size = 4"; echo "arch = any"
  echo "license = MIT"; for l in "$@"; do echo "$l"; done; } > $d/.PKGINFO
(cd $d && bsdtar -cf - .PKGINFO usr | zstd -q > $out/$name-$ver-1-any.pkg.tar.zst); rm -rf $d
