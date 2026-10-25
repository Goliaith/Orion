#!/bin/bash
# Orion's privileged helper, end to end: started as root, used by a normal
# user through Orion's own tools, and refusing what Orion never asks for,
# even when asked directly on its socket. Needs root, apt, python3 and
# network access for apt (it installs and removes the small package sl).
# With snapper and btrfs-progs, it snapshots a scratch btrfs volume too.
#
#   cargo build && sudo tests/helper/run.sh
set -u
[ "$(id -u)" = 0 ] || { echo "run this as root (sudo)"; exit 2; }
command -v apt-get >/dev/null || { echo "needs apt"; exit 2; }
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
chmod 755 "$work"
user=orion-hlp-user
outsider=orion-hlp-out
group=orion-hlp
helper_pid=
root_helper_pid=
snap=

cleanup() {
    [ -n "$helper_pid" ] && kill "$helper_pid" 2>/dev/null
    [ -n "$root_helper_pid" ] && kill "$root_helper_pid" 2>/dev/null
    rm -f /etc/orion-hlp.conf /etc/orion-hlp.link /etc/orion-hlp-2.conf
    if [ -n "$snap" ]; then
        snapper --no-dbus -c orion-hlp delete-config >/dev/null 2>&1
        umount "$work/snap" 2>/dev/null
    fi
    apt-get remove -y -q sl >/dev/null 2>&1
    userdel -r "$user" >/dev/null 2>&1
    userdel -r "$outsider" >/dev/null 2>&1
    groupdel "$group" >/dev/null 2>&1
    rm -rf "$work"
}
trap cleanup EXIT

groupadd "$group"
useradd -m -G "$group" -s /bin/sh "$user"
useradd -m -s /bin/sh "$outsider"
install -m 755 "$here/../../target/debug/orion" "$work/orion"

# A scratch btrfs volume with a snapper configuration, for the snapshots.
if command -v snapper >/dev/null && command -v mkfs.btrfs >/dev/null; then
    truncate -s 300M "$work/snap.img" && mkfs.btrfs -q "$work/snap.img" && mkdir "$work/snap" \
        && mount -o loop "$work/snap.img" "$work/snap" && snapper --no-dbus -c orion-hlp create-config "$work/snap" && snap=yes
fi

# The helper's policy: root's file. The user's: helper only, sl allowed.
cat > "$work/helper.toml" <<EOF
[helper]
socket = "$work/helper.sock"
group = "$group"
anchors = "$work/anchors"
[packages]
protected = ["dpkg", "apt", "curl"]   # curl: removable, but protected here
snapper_config = "orion-hlp"
EOF
cat > "$work/user.toml" <<EOF
[admin]
elevate = "helper"
[helper]
socket = "$work/helper.sock"
anchors = "$work/anchors"
[packages]
allowed = ["sl"]
snapper_config = "orion-hlp"
[tools]
pkg_remove = "allow"
pkg_rollback = "allow"
[audit]
enabled = false
EOF
chmod 644 "$work/helper.toml" "$work/user.toml"

"$work/orion" helper --config "$work/helper.toml" > "$work/helper.log" 2>&1 &
helper_pid=$!
for _ in $(seq 50); do [ -S "$work/helper.sock" ] && break; sleep 0.1; done

failed=0
expect() { # expect NAME OUTPUT PATTERN
    if grep -qF -- "$3" <<<"$2"; then echo "  ok    $1"; else echo "  FAIL  $1: wanted $3"; sed 's/^/    | /' <<<"$2"; failed=1; fi
}
as() { # as USER COMMAND…: run as that user, in a clean login environment
    local who=$1; shift
    su - "$who" -c "$*" 2>&1
}
orion() { as "$user" "$work/orion call --config $work/user.toml $*"; }
# A raw request on the socket, as Orion's client would send it.
raw() { # raw USER JSON
    as "$1" "python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall(sys.argv[2].encode()+b\"\\n\"); print(s.makefile().readline())' $work/helper.sock '$2'"
}

echo "== Orion's tools, through the helper"
ls -l "$work/helper.sock"
expect "the socket is root's and the group's only" "$(stat -c '%U %G %a' "$work/helper.sock")" "root $group 660"
expect "orion tools says root access goes through it" "$(as "$user" "$work/orion tools --config $work/user.toml")" "root access for the machine tools: the Orion helper at $work/helper.sock"
expect "update_check refreshes the lists through it" "$(orion update_check)" '"refreshed": true'
out=$(orion pkg_install "'{\"packages\":[\"sl\"]}'")
expect "pkg_install installs as root" "$out" '"elevated": true'
expect "pkg_install worked" "$out" '"ok": true'
expect "sl is installed" "$(dpkg-query -W -f '${Status}' sl 2>&1)" "install ok installed"
expect "the change is in the history" "$(orion pkg_history)" '"name": "sl"'
if [ -n "$snap" ]; then
    expect "snapshots were taken through the helper" "$out" '"config": "orion-hlp"'
else
    echo "  skip  snapshots (no snapper or btrfs here)"
fi
expect "pkg_rollback puts it back" "$(orion pkg_rollback "'{\"id\":1,\"method\":\"packages\"}'")" '"message": "change 1 is put back"'
expect "sl is gone" "$(dpkg-query -W -f '${Status}' sl 2>&1)" "no packages found"
expect "pkg_install again" "$(orion pkg_install "'{\"packages\":[\"sl\"]}'")" '"ok": true'
expect "pkg_remove removes it" "$(orion pkg_remove "'{\"packages\":[\"sl\"]}'")" '"ok": true'
expect "protected packages stay" "$(orion pkg_remove "'{\"packages\":[\"dpkg\"]}'")" "protected"

echo "== Asked directly, the helper refuses what Orion never asks for"
expect "a shell" "$(raw "$user" '{"program":"sh","args":["-c","id"],"stdin":null,"timeout_secs":10}')" '"refused":"the Orion helper doesn'"'"'t run sh"'
expect "an apt hook" "$(raw "$user" '{"program":"env","args":["DEBIAN_FRONTEND=noninteractive","apt-get","-o","APT::Update::Pre-Invoke::=id","update","-q"],"stdin":null,"timeout_secs":10}')" "isn't a command Orion makes for apt"
expect "removing a package the helper protects" "$(raw "$user" '{"program":"env","args":["DEBIAN_FRONTEND=noninteractive","apt-get","-y","-q","-o","Dpkg::Options::=--force-confold","remove","--","curl"],"stdin":null,"timeout_secs":60}')" "which is protected"
expect "a unit that runs a command" "$(raw "$user" '{"program":"tee","args":["--","/etc/systemd/system/cron.service.d/50-orion-gpu.conf"],"stdin":"[Service]\nExecStart=/bin/sh -c id\n","timeout_secs":10}')" "can't contain"
expect "a snapshot of another configuration" "$(raw "$user" '{"program":"snapper","args":["--no-dbus","-c","root","undochange","1..2"],"stdin":null,"timeout_secs":10}')" "refused"
expect "a snapper command Orion never makes" "$(raw "$user" '{"program":"snapper","args":["--no-dbus","-c","orion-hlp","delete","1"],"stdin":null,"timeout_secs":10}')" "refused"
if [ -n "$snap" ]; then
    expect "undoing an Orion snapshot pair" "$(raw "$user" '{"program":"snapper","args":["--no-dbus","-c","orion-hlp","undochange","1..2"],"stdin":null,"timeout_secs":60}')" '"status":0'
fi
expect "a SMART setting change" "$(raw "$user" '{"program":"smartctl","args":["--json=c","-a","--nocheck=standby","-s","off","/dev/sda"],"stdin":null,"timeout_secs":10}')" "refused"
expect "disk_health reads SMART through it" "$(orion disk_health)" '"disks"'
expect "a root shell on a console" "$(raw "$user" '{"program":"systemctl","args":["--no-ask-password","start","--","debug-shell.service"],"stdin":null,"timeout_secs":10}')" "protected"
# A real disk, so the refusal is about the options, not a missing device.
truncate -s 32M "$work/disk.img" && mkfs.ext4 -q "$work/disk.img" && loop=$(losetup -f --show "$work/disk.img" 2>/dev/null)
if [ -n "${loop:-}" ]; then
    expect "a mount that allows setuid" "$(raw "$user" '{"program":"mount","args":["-o","nodev","--","'"$loop"'","/mnt/orion-hlp"],"stdin":null,"timeout_secs":10}')" "the mount options for $loop are nosuid,nodev"
    expect "a mount over /etc" "$(raw "$user" '{"program":"mount","args":["-o","nosuid,nodev","--","'"$loop"'","/etc"],"stdin":null,"timeout_secs":10}')" "one of the system's own mount points"
    losetup -d "$loop"
else
    echo "  skip  mounts (no loop devices here)"
fi
expect "curl is still installed" "$(dpkg-query -W -f '${Status}' curl 2>&1)" "install ok installed"
expect "sl wasn't installed by any of that" "$(dpkg-query -W -f '${Status}' sl 2>&1)" "no packages found"

echo "== Checkpoints of the audit log"
cat > "$work/audited.toml" <<EOF
[admin]
elevate = "helper"
[helper]
socket = "$work/helper.sock"
anchors = "$work/anchors"
[audit]
path = "/home/$user/audit.jsonl"
EOF
chmod 644 "$work/audited.toml"
as "$user" "$work/orion call --config $work/audited.toml system_info >/dev/null; $work/orion call --config $work/audited.toml net_info >/dev/null"
expect "the helper takes a checkpoint" "$(as "$user" "$work/orion audit --config $work/audited.toml --checkpoint")" "checkpointed by the helper"
expect "the checkpoint is root's" "$(stat -c '%U %a' "$work/anchors/$(id -u "$user").jsonl")" "root 644"
expect "verify: the chain and the checkpoint hold" "$(as "$user" "$work/orion audit --config $work/audited.toml --verify")" "1 by the helper, all hold"
# Code running as the user rewrites its history: the checkpoint still knows.
as "$user" "sed -i 's/\"tool\":\"system_info\"/\"tool\":\"fs_list\"/' /home/$user/audit.jsonl"
out=$(as "$user" "$work/orion audit --config $work/audited.toml --verify; echo exit \$?")
expect "an edited entry breaks the chain" "$out" "doesn't follow entry 1"
expect "and verify fails" "$out" "exit 1"
as "$user" "rm /home/$user/audit.jsonl"
out=$(as "$user" "$work/orion audit --config $work/audited.toml --verify; echo exit \$?")
expect "deleting the whole log is caught too" "$out" "entries were removed"
expect "and verify fails" "$out" "exit 1"
expect "the user can't touch the checkpoints" "$(as "$user" "echo x >> $work/anchors/$(id -u "$user").jsonl" 2>&1)" "Permission denied"

echo "== Root work (root_run, sys_edit): only for the helper's root_users"
expect "root_run is off unless root lists the login" "$(raw "$user" '{"program":"orion-root-run","args":["id","-u"],"stdin":null,"timeout_secs":10}')" "root_users"
expect "sys_edit is off too" "$(raw "$user" '{"program":"orion-sys-edit","args":["/etc/orion-hlp.conf"],"stdin":"x\n","timeout_secs":10}')" "root_users"
expect "and wrote nothing" "$(ls /etc/orion-hlp.conf 2>&1)" "No such file"
# A second helper, whose own policy lists the user.
sed -e "s|$work/helper.sock|$work/root.sock|" -e "s|^group = .*|&\nroot_users = [\"$user\"]|" "$work/helper.toml" > "$work/root-helper.toml"
chmod 644 "$work/root-helper.toml"
"$work/orion" helper --config "$work/root-helper.toml" > "$work/root-helper.log" 2>&1 &
root_helper_pid=$!
for _ in $(seq 50); do [ -S "$work/root.sock" ] && break; sleep 0.1; done
root() { # root USER JSON: a raw request to the second helper
    as "$1" "python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall(sys.argv[2].encode()+b\"\\n\"); print(s.makefile().readline())' $work/root.sock '$2'"
}
expect "root_run runs a program as root" "$(root "$user" '{"program":"orion-root-run","args":["id","-u"],"stdin":null,"timeout_secs":10}')" '"stdout":"0\n"'
expect "only programs root put there" "$(root "$user" '{"program":"orion-root-run","args":["/home/'"$user"'/x"],"stdin":null,"timeout_secs":10}')" "doesn't exist"
expect "sys_edit creates a file" "$(root "$user" '{"program":"orion-sys-edit","args":["/etc/orion-hlp.conf","0600"],"stdin":"first\n","timeout_secs":10}')" "created /etc/orion-hlp.conf"
expect "root's, with the mode asked for" "$(stat -c '%U %a' /etc/orion-hlp.conf)" "root 600"
chmod 640 /etc/orion-hlp.conf
expect "and rewrites it in place" "$(root "$user" '{"program":"orion-sys-edit","args":["/etc/orion-hlp.conf"],"stdin":"second\n","timeout_secs":10}')" "rewrote /etc/orion-hlp.conf"
expect "keeping its mode" "$(stat -c '%a' /etc/orion-hlp.conf) $(cat /etc/orion-hlp.conf)" "640 second"
ln -s /etc/orion-hlp.conf /etc/orion-hlp.link
expect "not through a link" "$(root "$user" '{"program":"orion-sys-edit","args":["/etc/orion-hlp.link"],"stdin":"x\n","timeout_secs":10}')" "is a link to /etc/orion-hlp.conf"
expect "not in a home folder" "$(root "$user" '{"program":"orion-sys-edit","args":["/home/'"$user"'/.profile"],"stdin":"x\n","timeout_secs":10}')" "doesn't write under /home"
expect "not with .." "$(root "$user" '{"program":"orion-sys-edit","args":["/etc/../root/x"],"stdin":"x\n","timeout_secs":10}')" "no . or .."
expect "nor set-id modes" "$(root "$user" '{"program":"orion-sys-edit","args":["/etc/orion-hlp-2.conf","4755"],"stdin":"x\n","timeout_secs":10}')" "read and write bits only"
expect "someone outside the group still can't connect" "$(root "$outsider" '{"program":"orion-root-run","args":["id","-u"],"stdin":null,"timeout_secs":10}')" "Permission denied"
expect "it logs root work, and who asked" "$(cat "$work/root-helper.log")" "root_run: /usr/bin/id -u"
rm -f /etc/orion-hlp.conf /etc/orion-hlp.link /etc/orion-hlp-2.conf
printf '[admin]\nelevate = "helper"\n[helper]\nsocket = "%s"\n[root]\nsnapshots = "auto"\n[tools]\nproc_run = "deny"\n[audit]\nenabled = false\n' "$work/root.sock" > "$work/root-user.toml"
chmod 644 "$work/root-user.toml"
expect "through Orion, a person must approve it" "$(as "$user" "$work/orion call --config $work/root-user.toml root_run '{\"command\":\"id\"}'")" "needs interactive approval"
kill "$root_helper_pid" 2>/dev/null
root_helper_pid=

echo "== Only the helper's group may use it"
expect "someone outside the group can't connect" "$(as "$outsider" "$work/orion call --config $work/user.toml update_check")" "you may not use the Orion helper"

echo "== The helper's log"
log=$(cat "$work/helper.log")
expect "it logs what it ran, and who asked" "$log" "apt-get -y -q -o Dpkg::Options::=--force-confold install -- sl"
expect "it logs what it refused" "$log" "refused sh"

[ $failed = 0 ] && echo "all passed" || echo "some checks failed"
exit $failed
