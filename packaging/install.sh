#!/bin/sh
# Install Orion from a release: the binary, its systemd units and the
# example policy. Run as root for the whole machine (/usr/local, and the
# privileged helper's units), or as yourself for just you (~/.local):
#
#   curl -fsSL https://github.com/Goliaith/Orion/releases/latest/download/install.sh | sh
#   curl -fsSL https://github.com/Goliaith/Orion/releases/latest/download/install.sh | sudo sh
#
# Options:
#   --user          just for you, in ~/.local (the default when not root)
#   --version TAG   a release other than the latest (v0.2.0)
#   --uninstall     remove what this script installed
#
# ORION_DOWNLOAD=URL downloads from a mirror of the release files instead.
#
# Run from an unpacked release tarball, it installs that instead of
# downloading. Downloads are checked against the release's SHA256SUMS.
set -eu

repo=https://github.com/Goliaith/Orion
version=latest
mode=system
[ "$(id -u)" = 0 ] || mode=user
action=install
while [ $# -gt 0 ]; do
    case $1 in
        --user) mode=user ;;
        --version) version=${2:?--version needs a tag}; shift ;;
        --uninstall) action=uninstall ;;
        -h|--help) sed -n '2,18p' "$0"; exit 0 ;;
        *) echo "install.sh: unknown option $1" >&2; exit 2 ;;
    esac
    shift
done

if [ $mode = system ]; then
    [ "$(id -u)" = 0 ] || { echo "install.sh: installing for the whole machine needs root (sudo sh install.sh), or use --user" >&2; exit 1; }
    bindir=/usr/local/bin
    userunits=/etc/systemd/user
    sysunits=/etc/systemd/system
    docdir=/usr/local/share/doc/orion
else
    bindir=$HOME/.local/bin
    userunits=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
    sysunits=
    docdir=${XDG_DATA_HOME:-$HOME/.local/share}/doc/orion
fi
user_units="orion.service orion.socket orion-shell.service orion-voice.service orion-approvals.service"
sys_units="orion-helper.service orion-helper.socket"

reload() {
    command -v systemctl >/dev/null 2>&1 || return 0
    if [ $mode = system ]; then
        [ -d /run/systemd/system ] && systemctl daemon-reload || true
    else
        systemctl --user daemon-reload 2>/dev/null || true
    fi
}

if [ $action = uninstall ]; then
    if [ -n "$sysunits" ] && command -v systemctl >/dev/null 2>&1; then
        systemctl disable --now orion-helper.socket orion-helper.service 2>/dev/null || true
    fi
    rm -f "$bindir/orion" "$bindir/orion-kiosk"
    for u in $user_units; do rm -f "$userunits/$u"; done
    [ -z "$sysunits" ] || for u in $sys_units; do rm -f "$sysunits/$u"; done
    rm -rf "$docdir"
    reload
    echo "Orion is removed. Your policy and history stay in ~/.config/orion and ~/.local/state/orion."
    exit 0
fi

case $(uname -m) in
    x86_64|amd64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) echo "install.sh: there is no Orion release for $(uname -m) (x86_64 and aarch64 only); build it from source" >&2; exit 1 ;;
esac

# Where the release files are: next to this script, or downloaded.
here=$(cd "$(dirname "$0")" 2>/dev/null && pwd || echo .)
if [ -x "$here/orion" ] && [ -d "$here/systemd" ]; then
    src=$here
else
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    case $version in
        latest) base=$repo/releases/latest/download ;;
        *) base=$repo/releases/download/$version ;;
    esac
    base=${ORION_DOWNLOAD:-$base}
    fetch() { # fetch URL FILE
        case $1 in https://*) proto='=https' ;; *) proto='=http,https' ;; esac
        if command -v curl >/dev/null 2>&1; then curl -fsSL --proto "$proto" -o "$2" "$1"
        elif command -v wget >/dev/null 2>&1; then wget -q -O "$2" "$1"
        else echo "install.sh: needs curl or wget" >&2; exit 1
        fi
    }
    file=orion-$arch-linux.tar.gz
    echo "Downloading $base/$file"
    fetch "$base/$file" "$tmp/$file"
    fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"
    (cd "$tmp" && grep " $file\$" SHA256SUMS | sha256sum -c --quiet -) || { echo "install.sh: $file doesn't match SHA256SUMS" >&2; exit 1; }
    tar -C "$tmp" -xzf "$tmp/$file"
    src=$tmp/orion-$arch-linux
fi

# A distribution package owns /usr/bin/orion: two copies would only confuse.
if [ -x /usr/bin/orion ] && { dpkg -S /usr/bin/orion || rpm -qf /usr/bin/orion || pacman -Qo /usr/bin/orion; } >/dev/null 2>&1; then
    echo "note: a distribution package already installed /usr/bin/orion; $bindir/orion comes first on most PATHs"
fi

mkdir -p "$bindir" "$userunits" "$docdir"
install -m 755 "$src/orion" "$bindir/orion"
install -m 755 "$src/orion-kiosk" "$bindir/orion-kiosk"
for u in $user_units; do
    sed "s|@BINDIR@|$bindir|g" "$src/systemd/user/$u" > "$userunits/$u"
done
if [ -n "$sysunits" ]; then
    mkdir -p "$sysunits"
    for u in $sys_units; do
        sed "s|@BINDIR@|$bindir|g" "$src/systemd/system/$u" > "$sysunits/$u"
    done
    getent group orion >/dev/null || groupadd --system orion
fi
install -m 644 "$src"/docs/* "$src/orion.example.toml" "$docdir/"
reload

echo "Installed $("$bindir/orion" --version) in $bindir."
case :$PATH: in *:"$bindir":*) ;; *) echo "note: $bindir isn't on your PATH yet" ;; esac
cat <<EOF

Next:
  orion tools                                  what Orion may do here
  mkdir -p ~/.config/orion && cp $docdir/orion.example.toml ~/.config/orion/orion.toml
  systemctl --user enable --now orion.socket   the daemon, for agents and unattended updates
EOF
if [ $mode = system ]; then
    cat <<EOF
  systemctl enable --now orion-helper.socket   root work without sudo rules
  usermod -aG orion USER                       who may use it (then log in again)
EOF
else
    echo "  (the privileged helper needs a system-wide install: sudo sh install.sh)"
fi
echo "Guides: $docdir/install.md and operating.md"
