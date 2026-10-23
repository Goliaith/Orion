#!/bin/bash
# Orion's package and update tools against the real package managers of
# Fedora (dnf5), openSUSE Tumbleweed (zypper), Arch (pacman) and Debian
# (apt), in Docker: updates, the package history, rollbacks, and the
# daemon's unattended updates.
#
# No network is needed inside the containers: each gets a local repository
# of small test packages, where orion-demo and orion-sec have updates
# (orion-sec's is a security fix, with an advisory) and orion-new replaces
# orion-old. Needs docker, plus rpmbuild, createrepo_c, modifyrepo_c and
# dpkg-deb on the host (Debian/Ubuntu: apt install rpm createrepo-c dpkg-dev).
#
#   cargo build && tests/distros/run.sh [fedora] [suse] [arch] [debian]
set -u
here=$(cd "$(dirname "$0")" && pwd)
# The directory holding the orion binary to test (ORION_BIN: a release build, say).
bin=${ORION_BIN:-$(cd "$here/../.." && pwd)/target/debug}
work=$(mktemp -d)
chmod 755 "$work"   # apt reads the repository as its own _apt user
trap 'rm -rf "$work"; docker rm -f orion-test-fedora orion-test-suse orion-test-arch orion-test-debian >/dev/null 2>&1' EXIT
cp "$here"/*.sh "$here"/updateinfo.xml "$work"/
cd "$work"

# The test packages, as RPMs: version 1.0 to install first, and every
# version in the repository (so a rollback can find the old ones).
for p in "orion-demo 1.0" "orion-demo 2.0" "orion-sec 1.0" "orion-sec 1.1" "orion-old 1.0"; do ./mkrpm.sh $p; done
./mkrpm.sh orion-new 2.0 "Obsoletes: orion-old < 2.0" "Provides: orion-old = 2.0"
mkdir -p base repo && cp rpms/*-1.0-1.* base/ && mv rpms/* repo/
createrepo_c -q repo && modifyrepo_c --mdtype=updateinfo updateinfo.xml repo/repodata >/dev/null
# And as Debian packages.
for p in "orion-demo 1.0" "orion-sec 1.0"; do ./mkdeb.sh $p debbase; done
for p in "orion-demo 1.0" "orion-demo 2.0" "orion-sec 1.0" "orion-sec 1.1" "orion-new 2.0"; do ./mkdeb.sh $p debrepo; done
(cd debrepo && dpkg-scanpackages -m . 2>/dev/null > Packages)

failed=0
expect() { # expect NAME OUTPUT PATTERN...
    local name=$1 out=$2; shift 2
    for pat in "$@"; do
        if grep -qF -- "$pat" <<<"$out"; then echo "  ok    $name: $pat"; else echo "  FAIL  $name: $pat"; failed=1; fi
    done
}
refute() { # refute NAME OUTPUT PATTERN
    if grep -qF -- "$3" <<<"$2"; then echo "  FAIL  $1: not $3"; failed=1; else echo "  ok    $1: not $3"; fi
}
# The output of the call ("### tool args" header, then its result).
of() { # of OUTPUT HEADER [NTH]
    awk -v h="### $2" -v n="${3:-1}" '$0 == h { if (++seen == n) { getline; print; exit } }' <<<"$1"
}

start() { # start DISTRO IMAGE
    c=orion-test-$1
    echo "== $1 ($2)"
    docker rm -f $c >/dev/null 2>&1
    docker run -d --name $c -v "$work":/t -v "$bin":/opt/orion:ro "$2" sleep infinity >/dev/null || return 1
    docker exec $c bash -c "ln -sf /opt/orion/orion /usr/local/bin/orion; bash /t/setup-$1.sh"
}

run() { # run DISTRO IMAGE VERSIONS-COMMAND
    start $1 $2 || { failed=1; return; }
    local q=$3 out
    out=$(docker exec $c /t/inside.sh update_check "update_apply {}" "update_apply {\"full\":true}" "update_apply {}" \
        "pkg_install {\"packages\":[\"orion-nope\"]}" "pkg_remove {\"packages\":[\"orion-new\"]}" \
        pkg_history "pkg_rollback {\"id\":1}" "pkg_rollback {\"id\":2}" "sh $q" "pkg_rollback {\"id\":1}" "sh $q" \
        "pkg_rollback {\"id\":1}" "pkg_remove {\"packages\":[\"orion-old\"]}")
    local check=$(of "$out" "update_check {}")
    expect check "$check" '"count": 2' '"name": "orion-demo"'
    [ "$1" = arch ] || expect check "$check" '"security": 1'
    expect plan "$out" 'would remove orion-old'
    local full=$(of "$out" 'update_apply {"full":true}')
    expect full "$full" '"name": "orion-old", "version": "1.0-1"' '"name": "orion-new", "version": "2.0-1"' '"to": "1.1-1"' '"change": 1'
    expect again "$out" '"message": "nothing to update"'
    expect unknown "$(of "$out" 'pkg_install {"packages":["orion-nope"]}')" 'Error:'
    expect remove "$(of "$out" 'pkg_remove {"packages":["orion-new"]}')" '"removed": [ { "name": "orion-new"' '"change": 2'
    expect history "$(of "$out" "pkg_history {}")" '"count": 2' '"tool": "pkg_remove"' '"from": "1.0-1", "name": "orion-demo", "to": "2.0-1"'
    expect "newest first" "$(of "$out" 'pkg_rollback {"id":1}')" 'change 2' 'put that back first'
    expect "rollback of a removal" "$(of "$out" 'pkg_rollback {"id":2}')" '"method": "packages"' '"message": "change 2 is put back"'
    expect "it is back" "$(of "$out" "sh $q")" 'orion-new'
    expect "rollback of an update" "$(of "$out" 'pkg_rollback {"id":1}' 2)" '"message": "change 1 is put back"'
    local now=$(of "$out" "sh $q" 2)
    expect "old versions" "$now" 'orion-demo-1.0' 'orion-sec-1.0' 'orion-old-1.0'
    refute "old versions" "$now" 'orion-new-2.0'
    expect "only once" "$(of "$out" 'pkg_rollback {"id":1}' 3)" 'already put back, by change 4'
    unattended $1 '"change": 5' 'installed 2 updates (change 6)'
    # On a failure, what Orion actually said.
    [ $failed = 0 ] || sed 's/^/    | /' <<<"$out"
    docker rm -f $c >/dev/null
}

unattended() { # unattended DISTRO LAST-CALL-PATTERN STATUS-PATTERN
    local out
    out=$(docker exec $c /t/unattended.sh)
    expect unattended "$out" "$3"
}

# Debian: apt has no obsoleting, so orion-new is a plain new package there.
debian() {
    start debian debian:stable || { failed=1; return; }
    local q="dpkg-query -W -f '\${Package}-\${Version} ' orion-demo orion-sec orion-new" out
    out=$(docker exec $c /t/inside.sh update_check "update_apply {}" "pkg_install {\"packages\":[\"orion-new\"]}" pkg_history \
        "pkg_rollback {\"id\":1}" "sh $q" "pkg_rollback {\"id\":2}" "sh $q" "pkg_rollback {\"id\":1}")
    expect check "$(of "$out" "update_check {}")" '"count": 2' '"name": "orion-demo"'
    expect update "$(of "$out" 'update_apply {}')" '"to": "2.0-1"' '"change": 1' '"skipped": "snapper isn'"'"'t installed"'
    expect history "$(of "$out" "pkg_history {}")" '"count": 2' '"from": "1.0-1", "name": "orion-demo", "to": "2.0-1"'
    expect "rollback of an update" "$(of "$out" 'pkg_rollback {"id":1}')" '"reinstalled": [ "orion-demo=1.0-1"' '"message": "change 1 is put back"'
    expect "old versions" "$(of "$out" "sh $q")" 'orion-demo-1.0-1' 'orion-sec-1.0-1' 'orion-new-2.0-1'
    expect "rollback of an install" "$(of "$out" 'pkg_rollback {"id":2}')" '"removed": [ "orion-new" ]'
    refute "removed" "$(of "$out" "sh $q" 2)" 'orion-new-2.0-1'
    expect "only once" "$(of "$out" 'pkg_rollback {"id":1}' 2)" 'already put back, by change 3'
    unattended debian '' 'installed 2 updates (change 5)'
    [ $failed = 0 ] || sed 's/^/    | /' <<<"$out"
    docker rm -f $c >/dev/null
}

distros=${*:-fedora suse arch debian}
for d in $distros; do
    case $d in
        fedora) run fedora fedora:latest "rpm -q orion-demo orion-sec orion-old orion-new" ;;
        suse) run suse opensuse/tumbleweed "rpm -q orion-demo orion-sec orion-old orion-new" ;;
        arch) run arch archlinux:latest "pacman -Q orion-demo orion-sec orion-old orion-new | tr ' ' -" ;;
        debian) debian ;;
        *) echo "unknown distro $d"; failed=1 ;;
    esac
done
[ $failed = 0 ] && echo "all passed" || echo "some checks failed"
exit $failed
