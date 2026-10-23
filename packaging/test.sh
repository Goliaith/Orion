#!/bin/bash
# Install Orion's release files in clean containers and check they work:
# the .deb on Debian 11 (the oldest glibc we support) and Ubuntu, the .rpm
# on Rocky Linux 9, Fedora and openSUSE, the PKGBUILD on Arch, and
# install.sh from the tarball and by download. Needs docker and python3.
#
#   packaging/test.sh DIR VERSION [arm64]
#
# DIR holds what packaging/build.sh made (and SHA256SUMS). With arm64, the
# aarch64 .deb is also tried under emulation (binfmt/qemu must be set up).
set -u
[ $# -ge 2 ] || { sed -n '2,10p' "$0"; exit 2; }
dir=$(cd "$1" && pwd) version=$2 arm=${3:-}
here=$(cd "$(dirname "$0")" && pwd)
failed=0
names=()
trap 'docker rm -f "${names[@]}" >/dev/null 2>&1; [ -n "${server:-}" ] && kill $server 2>/dev/null' EXIT

check() { # check NAME IMAGE SCRIPT [docker run options...]
    local tag=$1 name=orion-pkg-$1 image=$2 script=$3; shift 3
    names+=("$name")
    echo "== $tag ($image)"
    docker rm -f "$name" >/dev/null 2>&1
    local out
    out=$(docker run --rm --name "$name" -v "$dir":/r:ro -v "$here":/p:ro -e V="$version" "$@" "$image" bash -c "$script" 2>&1)
    if grep -q '^PASS$' <<<"$out" && ! grep -q '^FAIL' <<<"$out"; then
        grep '^  ok' <<<"$out"
    else
        echo "  FAIL"; sed 's/^/    | /' <<<"$out"; failed=1
    fi
}

# What every installed Orion must do, with the binary and units at $1.
works='
t() { if eval "$2" >/dev/null 2>&1; then echo "  ok    $1"; else echo "FAIL  $1"; fi; }
w() { # w BINDIR USERUNITS SYSUNITS
    t "orion --version is $V" "$1/orion --version | grep -qx \"orion $V\""
    t "orion tools" "$1/orion tools"
    t "a tool call" "$1/orion call system_info {} | grep -q hostname"
    t "the daemon unit runs this orion" "grep -qx \"ExecStart=$1/orion daemon\" $2/orion.service"
    t "so does the helper unit" "grep -qx \"ExecStart=$1/orion helper\" $3/orion-helper.service"
    t "the orion group exists" "getent group orion"
}'
pkg="$works"'
d() { t "the example policy is in the docs" "test -f /usr/share/doc/orion/orion.example.toml"; }'

check debian11 debian:bullseye "$pkg"'
apt-get install -y -q --no-install-recommends /r/orion_${V}_amd64.deb >/dev/null 2>&1 || echo "FAIL  apt install"
w /usr/bin /usr/lib/systemd/user /usr/lib/systemd/system; d
apt-get remove -y -q orion >/dev/null 2>&1; t "apt remove takes it away" "! test -e /usr/bin/orion"
echo PASS'
check ubuntu ubuntu:24.04 "$pkg"'
apt-get install -y -q --no-install-recommends /r/orion_${V}_amd64.deb >/dev/null 2>&1 || echo "FAIL  apt install"
w /usr/bin /usr/lib/systemd/user /usr/lib/systemd/system
echo PASS'
check rocky9 rockylinux:9 "$pkg"'
dnf install -y -q --disablerepo="*" /r/orion-${V}-1.x86_64.rpm >/dev/null 2>&1 || echo "FAIL  dnf install"
w /usr/bin /usr/lib/systemd/user /usr/lib/systemd/system; d
dnf remove -y -q orion >/dev/null 2>&1; t "dnf remove takes it away" "! test -e /usr/bin/orion"
echo PASS'
check fedora fedora:latest "$pkg"'
dnf install -y -q --disablerepo="*" /r/orion-${V}-1.x86_64.rpm >/dev/null 2>&1 || echo "FAIL  dnf install"
w /usr/bin /usr/lib/systemd/user /usr/lib/systemd/system
echo PASS'
check suse opensuse/tumbleweed "$pkg"'
zypper -n --no-refresh install --no-recommends --allow-unsigned-rpm /r/orion-${V}-1.x86_64.rpm >/dev/null 2>&1; r=$?
# 106: some repositories could not be refreshed (no network here)
[ $r = 0 ] || [ $r = 106 ] || echo "FAIL  zypper install ($r)"
w /usr/bin /usr/lib/systemd/user /usr/lib/systemd/system
echo PASS'
# makepkg runs as a normal user, from the tarball next to the PKGBUILD.
check arch archlinux:latest "$works"'
pacman -Sy -q --noconfirm --needed fakeroot binutils >/dev/null 2>&1 || echo "FAIL  pacman -S fakeroot"
useradd -m b && mkdir /b && /p/pkgbuild.sh $V /r/SHA256SUMS > /b/PKGBUILD && cp /r/orion-x86_64-linux.tar.gz /b/orion-$V-x86_64-linux.tar.gz && chown -R b /b
su b -c "cd /b && makepkg -f >/tmp/makepkg.log 2>&1" || { echo "FAIL  makepkg"; cat /tmp/makepkg.log; }
pacman -U --noconfirm /b/orion-bin-$V-1-x86_64.pkg.tar.* >/dev/null 2>&1 || echo "FAIL  pacman -U"
w /usr/bin /usr/lib/systemd/user /usr/lib/systemd/system
echo PASS'
check tarball debian:bullseye "$works"'
mkdir /x && tar -C /x -xzf /r/orion-x86_64-linux.tar.gz && sh /x/orion-x86_64-linux/install.sh >/dev/null || echo "FAIL  install.sh"
w /usr/local/bin /etc/systemd/user /etc/systemd/system
useradd -m u
su u -c "sh /x/orion-x86_64-linux/install.sh >/dev/null" || echo "FAIL  install.sh as a user"
t "as a user, it goes in ~/.local" "test -x /home/u/.local/bin/orion && grep -q ExecStart=/home/u/.local/bin/orion /home/u/.config/systemd/user/orion.service"
su u -c "sh /x/orion-x86_64-linux/install.sh --uninstall >/dev/null"
t "and --uninstall takes it away" "! test -e /home/u/.local/bin/orion"
echo PASS'

# install.sh piped from a download, checked against SHA256SUMS.
port=$((20000 + RANDOM % 20000))
(cd "$dir" && exec python3 -m http.server $port --bind 127.0.0.1 >/dev/null 2>&1) &
server=$!
sleep 1
check download fedora:latest "$works"'
curl -fsS http://127.0.0.1:'$port'/install.sh | ORION_DOWNLOAD=http://127.0.0.1:'$port' sh >/dev/null || echo "FAIL  curl | sh"
w /usr/local/bin /etc/systemd/user /etc/systemd/system
echo PASS' --network host

if [ "$arm" = arm64 ]; then
    check arm64 debian:bullseye "$pkg"'
apt-get install -y -q --no-install-recommends /r/orion_${V}_arm64.deb >/dev/null 2>&1 || echo "FAIL  apt install"
w /usr/bin /usr/lib/systemd/user /usr/lib/systemd/system
echo PASS' --platform linux/arm64
fi

[ $failed = 0 ] && echo "all passed" || echo "some checks failed"
exit $failed
