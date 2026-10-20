#!/bin/bash
# Orion's package and update tools against the real package managers of
# Fedora (dnf5), openSUSE Tumbleweed (zypper) and Arch (pacman), in Docker.
#
# No network is needed inside the containers: each gets a local repository
# of small test packages, where orion-demo and orion-sec have updates
# (orion-sec's is a security fix, with an advisory) and orion-new replaces
# orion-old. Needs docker, plus rpmbuild, createrepo_c and modifyrepo_c on
# the host (Debian/Ubuntu: apt install rpm createrepo-c).
#
#   cargo build && tests/distros/run.sh [fedora] [suse] [arch]
set -u
here=$(cd "$(dirname "$0")" && pwd)
bin=$(cd "$here/../.." && pwd)/target/debug
work=$(mktemp -d)
trap 'rm -rf "$work"; docker rm -f orion-test-fedora orion-test-suse orion-test-arch >/dev/null 2>&1' EXIT
cp "$here"/*.sh "$here"/updateinfo.xml "$work"/
cd "$work"

# The test packages, as RPMs: version 1.0 to install first, the rest in a repository.
for p in "orion-demo 1.0" "orion-demo 2.0" "orion-sec 1.0" "orion-sec 1.1" "orion-old 1.0"; do ./mkrpm.sh $p; done
./mkrpm.sh orion-new 2.0 "Obsoletes: orion-old < 2.0" "Provides: orion-old = 2.0"
mkdir -p base repo && mv rpms/*-1.0-1.* base/ && mv rpms/* repo/
createrepo_c -q repo && modifyrepo_c --mdtype=updateinfo updateinfo.xml repo/repodata >/dev/null

failed=0
expect() { # expect NAME OUTPUT PATTERN...
    local name=$1 out=$2; shift 2
    for pat in "$@"; do
        if grep -qF -- "$pat" <<<"$out"; then echo "  ok    $name: $pat"; else echo "  FAIL  $name: $pat"; failed=1; fi
    done
}

run() { # run DISTRO IMAGE
    local c=orion-test-$1
    echo "== $1 ($2)"
    docker rm -f $c >/dev/null 2>&1
    docker run -d --name $c -v "$work":/t -v "$bin":/opt/orion:ro "$2" sleep infinity >/dev/null || { failed=1; return; }
    docker exec $c bash -c "ln -sf /opt/orion/orion /usr/local/bin/orion; bash /t/setup-$1.sh" || { failed=1; return; }
    local out
    out=$(docker exec $c /t/inside.sh update_check "update_apply {}" "update_apply {\"full\":true}" "update_apply {}" \
        "pkg_install {\"packages\":[\"orion-nope\"]}" "pkg_remove {\"packages\":[\"orion-new\"]}")
    local check=$(sed -n '/^### update_check/{n;p}' <<<"$out")
    expect check "$check" '"count": 2' '"name": "orion-demo"'
    [ "$1" = arch ] || expect check "$check" '"security": 1'
    expect plan "$out" 'would remove orion-old'
    expect full "$(sed -n '/^### update_apply {"full":true}/{n;p}' <<<"$out")" \
        '"name": "orion-old", "version": "1.0-1"' '"name": "orion-new", "version": "2.0-1"' '"to": "1.1-1"'
    expect again "$out" '"message": "nothing to update"'
    expect unknown "$(sed -n '/^### pkg_install/{n;p}' <<<"$out")" 'Error:'
    expect remove "$(sed -n '/^### pkg_remove/{n;p}' <<<"$out")" '"removed": [ { "name": "orion-new"'
    # On a failure, what Orion actually said.
    [ $failed = 0 ] || sed 's/^/    | /' <<<"$out"
    docker rm -f $c >/dev/null
}

distros=${*:-fedora suse arch}
for d in $distros; do
    case $d in
        fedora) run fedora fedora:latest ;;
        suse) run suse opensuse/tumbleweed ;;
        arch) run arch archlinux:latest ;;
        *) echo "unknown distro $d"; failed=1 ;;
    esac
done
[ $failed = 0 ] && echo "all passed" || echo "some checks failed"
exit $failed
