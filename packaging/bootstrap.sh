#!/bin/sh
# Install or update Orion from source, straight from the private repository:
# the build tools, Rust, the binary, the privileged helper and the daemon.
# Run it as yourself; it asks sudo for the parts that need root. With the
# GitHub CLI signed in (gh auth login), one command does it all:
#
#   gh api -H 'Accept: application/vnd.github.raw' repos/Goliaith/Orion/contents/packaging/bootstrap.sh | sh
#
# or, in a checkout:  sh packaging/bootstrap.sh
# Run it again to update: it pulls, rebuilds and restarts what's running.
#
# Options:
#   --dir DIR       where the source lives (default: the checkout this script
#                   is in, else ~/Orion)
#   --branch NAME   the branch to build (default: main)
#   --no-helper     skip the privileged helper (root work then needs sudo rules)
#   --no-daemon     skip the background service
#   --skip-deps     don't install build tools with the package manager
set -eu

repo=Goliaith/Orion
dir=
branch=main
helper=yes
daemon=yes
deps=yes
while [ $# -gt 0 ]; do
    case $1 in
        --dir) dir=${2:?--dir needs a directory}; shift ;;
        --branch) branch=${2:?--branch needs a name}; shift ;;
        --no-helper) helper=no ;;
        --no-daemon) daemon=no ;;
        --skip-deps) deps=no ;;
        -h|--help) sed -n '2,21p' "$0" 2>/dev/null || echo "see packaging/bootstrap.sh"; exit 0 ;;
        *) echo "bootstrap.sh: unknown option $1" >&2; exit 2 ;;
    esac
    shift
done

say() { printf '\n==> %s\n' "$*"; }
die() { echo "bootstrap.sh: $*" >&2; exit 1; }

# Everything runs from main, so a shell reading this script from a pipe has
# read all of it before any command can touch stdin.
main() {
    [ "$(id -u)" != 0 ] || die "run this as yourself, not root; it uses sudo where it must"
    [ "$(uname -s)" = Linux ] || die "Orion runs on Linux only"

    if [ $deps = yes ]; then
        build_tools
    fi
    get_source
    get_rust

    say "Building Orion (a few minutes the first time)"
    cargo install --locked --path "$dir"
    bin=$HOME/.cargo/bin/orion

    [ $helper = no ] || install_helper
    [ $daemon = no ] || install_daemon

    say "Done: $("$bin" --version)"
    "$bin" status 2>/dev/null || true
    next_steps
}

build_tools() {
    missing=
    for c in cc git curl; do command -v $c >/dev/null 2>&1 || missing="$missing $c"; done
    [ -n "$missing" ] || return 0
    say "Installing build tools (missing:$missing)"
    if command -v dnf >/dev/null 2>&1; then sudo dnf install -y gcc git curl
    elif command -v apt-get >/dev/null 2>&1; then sudo apt-get update && sudo apt-get install -y build-essential git curl
    elif command -v zypper >/dev/null 2>&1; then sudo zypper --non-interactive install gcc git curl
    elif command -v pacman >/dev/null 2>&1; then sudo pacman -S --needed --noconfirm base-devel git curl
    else die "install a C compiler, git and curl, then run this again"
    fi
}

get_source() {
    if [ -z "$dir" ]; then
        here=$(cd "$(dirname "$0")/.." 2>/dev/null && pwd || true)
        if [ -n "$here" ] && grep -qs '^name = "orion"' "$here/Cargo.toml"; then dir=$here; else dir=$HOME/Orion; fi
    fi
    if [ -d "$dir/.git" ]; then
        if [ -n "$(git -C "$dir" status --porcelain --untracked-files=no)" ]; then
            say "$dir has local changes: building it as it is, without updating"
            return 0
        fi
        say "Updating $dir ($branch)"
        git -C "$dir" fetch origin "$branch"
        git -C "$dir" checkout -q "$branch" 2>/dev/null || git -C "$dir" checkout -q -b "$branch" "origin/$branch"
        git -C "$dir" merge --ff-only -q "origin/$branch" || die "$dir has commits that aren't on origin/$branch; sort that out, then run this again"
        return 0
    fi
    [ ! -e "$dir" ] || die "$dir exists but isn't a git checkout; pass --dir somewhere else"
    say "Downloading the source into $dir"
    # The repository is private: the GitHub CLI's sign-in gets git in.
    if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
        gh auth setup-git
        gh repo clone "$repo" "$dir" -- --branch "$branch"
    else
        echo "(the GitHub CLI isn't signed in, so git may ask for your GitHub name and a token)"
        git clone --branch "$branch" "https://github.com/$repo.git" "$dir"
    fi
}

get_rust() {
    # shellcheck disable=SC1091
    [ ! -f "$HOME/.cargo/env" ] || . "$HOME/.cargo/env"
    need=$(sed -n 's/^rust-version *= *"\([0-9.]*\)".*/\1/p' "$dir/Cargo.toml")
    if command -v cargo >/dev/null 2>&1 && rust_at_least "$need"; then
        return 0
    fi
    if command -v rustup >/dev/null 2>&1; then
        say "Updating Rust (Orion needs $need or newer)"
        rustup update stable
    else
        say "Installing Rust with rustup"
        rustup_init=$(mktemp)
        curl --proto '=https' --tlsv1.2 -sSf -o "$rustup_init" https://sh.rustup.rs || die "couldn't download rustup from sh.rustup.rs"
        sh "$rustup_init" -y --profile minimal
        rm -f "$rustup_init"
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
    fi
    rust_at_least "$need" || die "Rust is still older than $need"
}

rust_at_least() { # rust_at_least 1.95
    have=$(rustc --version 2>/dev/null | cut -d' ' -f2)
    [ -n "$have" ] || return 1
    hmaj=${have%%.*}; hrest=${have#*.}; hmin=${hrest%%.*}
    nmaj=${1%%.*}; nrest=${1#*.}; nmin=${nrest%%.*}
    [ "$hmaj" -gt "$nmaj" ] || { [ "$hmaj" -eq "$nmaj" ] && [ "$hmin" -ge "$nmin" ]; }
}

has_systemd() { [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; }

install_helper() {
    say "Installing the privileged helper (sudo may ask for your password)"
    # The helper runs as root from /usr/local/bin, never from your home.
    sudo install -m 755 "$bin" /usr/local/bin/orion
    getent group orion >/dev/null || sudo groupadd --system orion
    if ! id -nG "$(id -un)" | tr ' ' '\n' | grep -qx orion; then
        sudo usermod -aG orion "$(id -un)"
        relogin=yes
    fi
    sudo install -m 644 "$dir/contrib/orion-helper.socket" "$dir/contrib/orion-helper.service" /etc/systemd/system/
    if has_systemd; then
        sudo systemctl daemon-reload
        sudo systemctl enable --now orion-helper.socket
        # A helper that is running keeps the old binary until it restarts.
        sudo systemctl try-restart orion-helper.service
    else
        echo "(systemd isn't running here, so the helper isn't started)"
    fi
}

install_daemon() {
    if ! has_systemd || ! systemctl --user show-environment >/dev/null 2>&1; then
        say "Skipping the background service: there is no systemd user session here"
        return 0
    fi
    say "Installing the background service"
    units=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
    mkdir -p "$units"
    install -m 644 "$dir/contrib/orion.socket" "$dir/contrib/orion.service" "$units/"
    systemctl --user daemon-reload
    systemctl --user enable --now orion.socket
    systemctl --user try-restart orion.service
}

next_steps() {
    echo
    if [ "${relogin:-}" = yes ]; then
        echo "Log out and back in once, so you're in the orion group the helper answers."
    fi
    if [ ! -f "$HOME/.config/orion/orion.toml" ]; then
        echo "Orion has no policy yet; it runs on the defaults. To start one:"
        echo "  mkdir -p ~/.config/orion && cp $dir/orion.example.toml ~/.config/orion/orion.toml"
    fi
    echo "Connect a model (Claude, Gemini or Ollama): see $dir/docs/install.md, section 4."
    echo "To update later, run this again: sh $dir/packaging/bootstrap.sh"
}

main "$@" </dev/null
