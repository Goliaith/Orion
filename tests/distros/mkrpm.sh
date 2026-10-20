#!/bin/bash
# mkrpm.sh NAME VERSION [extra spec lines...] -> rpms/NAME-VERSION-1.noarch.rpm
set -e
name=$1 ver=$2; shift 2
top=$(mktemp -d)
mkdir -p $top/SPECS
{
echo "Name: $name"; echo "Version: $ver"; echo "Release: 1"; echo "Summary: Orion test package $name"
echo "License: MIT"; echo "BuildArch: noarch"
for l in "$@"; do echo "$l"; done
echo "%description"; echo "Test package."
echo "%install"; echo "mkdir -p %{buildroot}/usr/share/$name"; echo "echo $ver > %{buildroot}/usr/share/$name/version"
echo "%files"; echo "/usr/share/$name/version"
} > $top/SPECS/p.spec
rpmbuild -bb --quiet --define "_topdir $top" --define "_binary_payload w9.gzdio" $top/SPECS/p.spec
mkdir -p rpms; cp $top/RPMS/noarch/*.rpm rpms/; rm -rf $top
