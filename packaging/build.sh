#!/bin/bash
# Orion's release files for one architecture, from a built binary:
#
#   orion-ARCH-linux.tar.gz    the binary, units, docs and install.sh
#   orion_VERSION_DEB.deb      for Debian and Ubuntu (DEB: amd64, arm64)
#   orion-VERSION-1.ARCH.rpm   for Fedora, RHEL and openSUSE
#
#   packaging/build.sh VERSION ARCH BINARY OUTDIR      (ARCH: x86_64 or aarch64)
#
# Needs dpkg-deb and rpmbuild. The binary should be built against an old
# glibc so it runs everywhere (the release workflow uses glibc 2.31).
set -euo pipefail
[ $# = 4 ] || { sed -n '2,10p' "$0"; exit 2; }
version=$1 arch=$2 binary=$3 out=$4
here=$(cd "$(dirname "$0")" && pwd)
top=$(cd "$here/.." && pwd)
case $arch in
    x86_64) deb_arch=amd64 ;;
    aarch64) deb_arch=arm64 ;;
    *) echo "unknown architecture $arch (x86_64 or aarch64)" >&2; exit 2 ;;
esac
# An SPDX license from Cargo.toml, or proprietary with license-file.
license=$(sed -n 's/^license *= *"\(.*\)"/\1/p' "$top/Cargo.toml")
license=${license:-LicenseRef-Proprietary}
summary=$(sed -n 's/^description *= *"\(.*\)"/\1/p' "$top/Cargo.toml")
mkdir -p "$out"
out=$(cd "$out" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The tarball. Its units say @BINDIR@ where they name the binary; whoever
# installs them (install.sh, the packages below, the PKGBUILD) fills it in.
t=$work/orion-$arch-linux
mkdir -p "$t/systemd/user" "$t/systemd/system" "$t/docs"
install -m 755 "$binary" "$t/orion"
install -m 755 "$top/contrib/orion-kiosk" "$here/install.sh" "$t/"
install -m 644 "$top/orion.example.toml" "$here/sysusers.conf" "$top/LICENSE" "$t/"
install -m 644 "$top/README.md" "$top/docs/install.md" "$top/docs/operating.md" "$t/docs/"
for unit in "$top"/contrib/*.service "$top"/contrib/*.socket; do
    case $(basename "$unit") in orion-helper.*) kind=system ;; *) kind=user ;; esac
    sed -e 's|%h/\.cargo/bin/orion|@BINDIR@/orion|g' -e 's|/usr/local/bin/orion|@BINDIR@/orion|g' "$unit" > "$t/systemd/$kind/$(basename "$unit")"
done
tar -C "$work" --owner=0 --group=0 -czf "$out/orion-$arch-linux.tar.gz" "orion-$arch-linux"

# The tree both packages install.
stage() { # stage ROOT
    local r=$1
    install -Dm 755 "$t/orion" "$r/usr/bin/orion"
    install -Dm 755 "$t/orion-kiosk" "$r/usr/bin/orion-kiosk"
    install -Dm 644 "$t/sysusers.conf" "$r/usr/lib/sysusers.d/orion.conf"
    for kind in user system; do
        mkdir -p "$r/usr/lib/systemd/$kind"
        for unit in "$t/systemd/$kind"/*; do
            sed 's|@BINDIR@|/usr/bin|g' "$unit" > "$r/usr/lib/systemd/$kind/$(basename "$unit")"
        done
    done
    mkdir -p "$r/usr/share/doc/orion"
    install -m 644 "$t"/docs/* "$t/orion.example.toml" "$r/usr/share/doc/orion/"
    install -m 644 "$t/LICENSE" "$r/usr/share/doc/orion/copyright"
    install -Dm 644 "$t/LICENSE" "$r/usr/share/licenses/orion/LICENSE"
}

# The .deb.
d=$work/deb
stage "$d"
mkdir -p "$d/DEBIAN"
cat > "$d/DEBIAN/control" <<EOF
Package: orion
Version: $version
Architecture: $deb_arch
Maintainer: Orion <orion@users.noreply.github.com>
Installed-Size: $(du -sk "$d/usr" | cut -f1)
Depends: libc6 (>= 2.31)
Recommends: bubblewrap, libnotify-bin
Suggests: snapper
Section: admin
Priority: optional
Homepage: https://github.com/Goliaith/Orion
Description: $summary
 Orion gives AI agents and assistants a policy-checked, sandboxed and
 audited way to look after a Linux machine: files, processes, services,
 packages and updates, network, disks and GPUs.
EOF
cat > "$d/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = configure ]; then
    # The group whose members may use the privileged helper.
    if command -v systemd-sysusers >/dev/null 2>&1; then systemd-sysusers orion.conf || true; fi
    getent group orion >/dev/null || groupadd --system orion
    [ -d /run/systemd/system ] && systemctl daemon-reload || true
fi
EOF
cat > "$d/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = remove ] && [ -d /run/systemd/system ]; then
    systemctl stop orion-helper.socket orion-helper.service 2>/dev/null || true
fi
EOF
chmod 755 "$d/DEBIAN/postinst" "$d/DEBIAN/prerm"
dpkg-deb -Zxz --root-owner-group -b "$d" "$out/orion_${version}_$deb_arch.deb" >/dev/null

# The .rpm, from the same tree.
r=$work/rpm
mkdir -p "$r/SPECS"
cat > "$r/SPECS/orion.spec" <<EOF
Name: orion
Version: $version
Release: 1
Summary: $summary
License: $license
URL: https://github.com/Goliaith/Orion
Requires: glibc >= 2.31
Recommends: bubblewrap
Recommends: libnotify
Suggests: snapper

%description
Orion gives AI agents and assistants a policy-checked, sandboxed and
audited way to look after a Linux machine: files, processes, services,
packages and updates, network, disks and GPUs.

%install
cp -a $work/tree/. %{buildroot}/

%pre
getent group orion >/dev/null || groupadd --system orion
exit 0

%post
[ -d /run/systemd/system ] && systemctl daemon-reload || true

%preun
if [ \$1 -eq 0 ] && [ -d /run/systemd/system ]; then
    systemctl stop orion-helper.socket orion-helper.service 2>/dev/null || true
fi

%files
/usr/bin/orion
/usr/bin/orion-kiosk
/usr/lib/sysusers.d/orion.conf
/usr/lib/systemd/user/*
/usr/lib/systemd/system/orion-helper.service
/usr/lib/systemd/system/orion-helper.socket
%doc /usr/share/doc/orion
%license /usr/share/licenses/orion/LICENSE
EOF
stage "$work/tree"
rpmbuild -bb --quiet --target "$arch-linux" \
    --define "_topdir $r" --define "debug_package %{nil}" --define "__os_install_post %{nil}" \
    --define "_build_id_links none" --define "_binary_payload w9.gzdio" "$r/SPECS/orion.spec"
cp "$r"/RPMS/*/orion-*.rpm "$out/"

ls -1 "$out"
