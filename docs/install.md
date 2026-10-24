# Installing Orion

This guide takes a Linux machine from nothing to a working Orion: installing it, giving it a model to talk to, the optional parts (sandbox, voice, the graphical shell, network and disk tools), root access for the machine tools, and running it as a service. [Operating Orion](operating.md) covers using it day to day.

- [What you need](#what-you-need)
- [1. Install](#1-install)
- [2. Check it works](#2-check-it-works)
- [3. Write a policy](#3-write-a-policy)
- [4. Connect a model](#4-connect-a-model)
- [5. Optional parts](#5-optional-parts)
- [6. Root access for the machine tools](#6-root-access-for-the-machine-tools)
- [7. Run it as a service](#7-run-it-as-a-service)
- [8. Connect other AI agents (MCP)](#8-connect-other-ai-agents-mcp)
- [Upgrading](#upgrading)
- [Uninstalling](#uninstalling)
- [Making a release](#making-a-release)

## What you need

| | Required | Notes |
|---|---|---|
| Linux | yes | Any distribution. The tools read `/proc` and `/sys` directly, so Orion doesn't build or run on macOS or Windows. |
| glibc 2.31 or newer | for a release | Debian 11, Ubuntu 20.04, RHEL 9, and every current distribution. On x86_64 or aarch64. Older or musl-based systems build from source. |
| Rust 1.95 or newer | to build from source | Older compilers stop with "rustc … is not supported". Distribution packages are often older; [rustup](https://rustup.rs) always works. |
| A C compiler | to build from source | One dependency (`ring`, for TLS) compiles C. |
| A model | to chat | Claude (an API key), a model on this machine (Ollama, llama.cpp, vLLM or LM Studio), or both. |
| systemd | for some tools | The service tools, `gpu_assign` and the service units in `contrib/`. Everything else works without it. |

What works on which distribution:

| | Debian, Ubuntu | Fedora, RHEL 8+ | openSUSE, SLES | Arch |
|---|---|---|---|---|
| Package and update tools | apt | dnf | zypper | pacman |
| Tested on | Ubuntu 24.04, Debian | Fedora 44 | Tumbleweed | Arch |
| Release package | `.deb` | `.rpm` | `.rpm` | PKGBUILD |
| Snapshots before changes | with snapper on btrfs | with snapper on btrfs | by default | with snapper on btrfs |

Other distributions (Alpine, Void, Gentoo, NixOS, image-based systems such as Fedora Silverblue) run everything except the package and update tools. The network tools need NetworkManager, and the firewall tools ufw or firewalld.

## 1. Install

Install a release, or build from source. A release is ready to run: the binary (about 10 MB), its systemd units, the `orion` group for the [privileged helper](#the-privileged-helper-recommended), and the guides and example policy in `/usr/share/doc/orion`.

### From a release

Every [release](https://github.com/Goliaith/Orion/releases) has packages for x86_64 and aarch64. Pick your distribution; `V` is the release you want:

```sh
V=0.1.0

# Debian, Ubuntu (on ARM: _arm64.deb)
curl -LO https://github.com/Goliaith/Orion/releases/download/v$V/orion_${V}_amd64.deb
sudo apt install ./orion_${V}_amd64.deb

# Fedora, RHEL 9, Rocky, Alma (on ARM: .aarch64.rpm)
curl -LO https://github.com/Goliaith/Orion/releases/download/v$V/orion-$V-1.x86_64.rpm
sudo dnf install ./orion-$V-1.x86_64.rpm

# openSUSE
curl -LO https://github.com/Goliaith/Orion/releases/download/v$V/orion-$V-1.x86_64.rpm
sudo zypper install --allow-unsigned-rpm ./orion-$V-1.x86_64.rpm

# Arch: the PKGBUILD packages the release binary (orion-bin)
mkdir orion-bin && cd orion-bin
curl -LO https://github.com/Goliaith/Orion/releases/download/v$V/PKGBUILD
makepkg -si
```

On any other distribution, or to skip the package manager, the install script installs the latest release, checked against the release's `SHA256SUMS`:

```sh
curl -fsSL https://github.com/Goliaith/Orion/releases/latest/download/install.sh | sudo sh   # for every user
curl -fsSL https://github.com/Goliaith/Orion/releases/latest/download/install.sh | sh        # just you
```

As root it installs `/usr/local/bin/orion`, the user units in `/etc/systemd/user`, the helper's units in `/etc/systemd/system`, and the `orion` group. Without root it installs into `~/.local/bin` and `~/.config/systemd/user`, and leaves out the helper, which needs root. `--version v0.1.0` picks a release, and `--uninstall` removes what it installed.

The packages aren't signed. To check one by hand, download `SHA256SUMS` beside it and run `sha256sum -c --ignore-missing SHA256SUMS`.

With a package or the install script the systemd units are already in place, so in [7. Run it as a service](#7-run-it-as-a-service) you skip the `cp` lines.

### From the private repository, in one command

`packaging/bootstrap.sh` does the whole from-source install below, and the root helper and daemon from sections 6 and 7, as yourself, asking sudo only where it must. It needs the GitHub CLI signed in, because the repository is private:

```sh
sudo dnf install -y gh        # or apt, zypper, pacman
gh auth login
gh api -H 'Accept: application/vnd.github.raw' repos/Goliaith/Orion/contents/packaging/bootstrap.sh | sh
```

It installs the build tools and Rust if they are missing, clones the source into `~/Orion`, builds it, installs the helper (copying the binary to `/usr/local/bin`, creating the `orion` group and adding you to it) and starts the daemon. Run it again, as `sh ~/Orion/packaging/bootstrap.sh`, to update: it pulls `main`, rebuilds and restarts the helper and the daemon. A checkout with local changes is built as it is, without pulling. The first time, it ends by running `orion setup` (section 4) to connect a model. `--no-helper`, `--no-daemon`, `--no-setup`, `--branch NAME`, `--dir DIR` and `--skip-deps` change what it does.

### From source

Install the build tools. Pick your distribution:

```sh
sudo apt install build-essential git curl        # Debian, Ubuntu
sudo dnf install gcc git curl                    # Fedora, RHEL
sudo zypper install gcc git curl                 # openSUSE
sudo pacman -S --needed base-devel git curl      # Arch
```

Install Rust with rustup, unless `rustc --version` already says 1.95 or newer:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
```

Get the source and install the `orion` binary:

```sh
git clone https://github.com/Goliaith/Orion.git
cd Orion
cargo install --path .        # builds a release binary into ~/.cargo/bin/orion
```

That is one self-contained binary, about 11 MB. `~/.cargo/bin` is on your `PATH` after rustup's setup. The systemd units in `contrib/` expect it there.

To install it for every user instead:

```sh
cargo build --release
sudo install -m 755 target/release/orion /usr/local/bin/orion
```

If you do that, change `ExecStart=%h/.cargo/bin/orion` to `ExecStart=/usr/local/bin/orion` in the units you copy from `contrib/`. (`packaging/install.sh`, run from a release tarball, does all of that for you.)

To run the tests (optional, about a minute):

```sh
cargo test
```

## 2. Check it works

```sh
orion --version
orion tools
```

`orion tools` lists every tool, its risk level and what the policy decides for it, and ends with the sandbox it will use:

```
TOOL         RISK     POLICY DESCRIPTION
system_info  read     allow  Describe this machine: hostname, OS, kernel, CPU, memory, load average and uptime
…
programs run sandboxed: bubblewrap, read-only outside the writable roots, private paths hidden, no network, no GPU
```

Call a tool yourself, exactly as the AI would:

```sh
orion call system_info
orion call disk_list
orion call update_check '{"refresh": false}'
```

No model is involved yet. These are the same tools, policy and audit log the AI gets.

## 3. Write a policy

Orion works with no policy file: the AI may look at the machine, and anything that changes it asks you first. To change that, start from the example:

```sh
mkdir -p ~/.config/orion
cp orion.example.toml ~/.config/orion/orion.toml
```

Every key in it is optional, and the values shown are the defaults. Orion reads `~/.config/orion/orion.toml`, or the file in `ORION_CONFIG`, or the one given with `--config`. A misspelled key is an error, so a typo can't silently do nothing. Run `orion tools` after each edit to see the result.

Settings worth deciding on now:

- **`[fs] write_roots`**: where the AI may write files. The default is `~/orion-workspace`.
- **`[proc] allowed_commands`**: programs the AI may run without asking, such as `["df", "free", "uname", "git"]`.
- **`[packages] updates`**: `"security"` lets security updates install without asking.
- **`[chat] provider`**: `"auto"` (route each message), `"claude"` or `"local"`.

[Operating Orion](operating.md#tuning-the-policy) has ready-made policies for common setups.

## 4. Connect a model

You need at least one. With both, Orion routes each message: private and simple work stays on this machine, complex work goes to Claude.

The quick way:

```sh
orion setup
```

It asks which model to use (Claude, Gemini, or a server on this machine), takes the key with the typing hidden, drops any spaces or invisible characters the paste brought along, and checks it with a live call. Then it picks a model the key actually offers, asks it to say ok, and writes `~/.config/orion/orion.toml` (changing only the model settings, comments kept, the old file saved as `orion.toml.bak`) and `~/.config/orion/env` (mode 600). Every `orion` command reads that env file at start, so the key works at once, without a new shell. If a running service needs restarting to see it, setup offers to. With Claude, it also offers a local server it finds, so private and simple messages stay on this machine.

`orion setup --provider gemini --model gemini-3.8-flash` skips the questions it can; `--yes` takes every default and a key already in the environment (`GEMINI_API_KEY=… orion setup --provider gemini --yes`).

The sections below are the same steps by hand.

### Claude

Create an API key at [console.anthropic.com](https://console.anthropic.com) and export it:

```sh
export ANTHROPIC_API_KEY=sk-ant-...
orion chat --claude
```

Put the `export` line in your shell profile to keep it. For the services in step 7, put it in `~/.config/orion/env` instead (one `VAR=value` per line), readable only by you:

```sh
install -m 600 /dev/null ~/.config/orion/env
echo 'ANTHROPIC_API_KEY=sk-ant-...' >> ~/.config/orion/env
```

`ANTHROPIC_AUTH_TOKEN` (an OAuth access token) works in place of a key, and `ANTHROPIC_BASE_URL` points Orion at a different endpoint. The model is `claude-opus-5-5` unless you set `[chat.claude] model`.

### A model on this machine

Any server with the OpenAI-compatible chat API works. Ollama is the simplest:

```sh
curl -fsSL https://ollama.com/install.sh | sh     # installs and starts the Ollama service
ollama pull qwen3:8b
orion chat --local
```

- **Pick a model trained for tool calling:** Qwen3, Llama 3.1+, Mistral Small 3 or gpt-oss. Models under about 7B often call tools badly.
- **Give it room:** Orion's tools fill a small context window fast. For Ollama, set `OLLAMA_CONTEXT_LENGTH=32768` in its service environment (`sudo systemctl edit ollama`, then `Environment=OLLAMA_CONTEXT_LENGTH=32768` under `[Service]`).
- **Other servers:** point `[chat.local] base_url` at them. llama.cpp's `llama-server --jinja` is on `http://127.0.0.1:8080/v1`, vLLM on `:8000/v1`, LM Studio on `:1234/v1`.
- **A small judge for routing:** `[chat.router] classifier_model = "qwen3:1.7b"` makes automatic routing faster. Pull it with `ollama pull qwen3:1.7b`.

### Gemini (free tier)

Google's Gemini speaks the same API, and its free tier needs no payment. Create a key at [aistudio.google.com](https://aistudio.google.com/apikey), put it in `~/.config/orion/env` as `GEMINI_API_KEY=...`, and point the local backend at Google:

```toml
[chat]
provider = "local"

[chat.local]
base_url = "https://generativelanguage.googleapis.com/v1beta/openai"
model = "gemini-3.8-flash"            # or a flash-lite from the list below
api_key_env = "GEMINI_API_KEY"
reasoning_effort = "low"              # less thinking, faster answers; "minimal", "low", "medium" or "high"
```

- **Pick a chat model:** list what your key offers with `curl -s -H "Authorization: Bearer $GEMINI_API_KEY" https://generativelanguage.googleapis.com/v1beta/openai/models`, and take a plain `flash` or `flash-lite` one. The `tts` and `image` variants make speech and pictures, and can't chat or use tools.
- **Free-tier limits are tight,** a few requests a minute, and each tool Orion runs is one more. When Google says to slow down, Orion waits and says so (`rate limited, retrying in 17s (1 of 3)`). Flash-Lite has the most generous limits.
- **Private stays on this machine.** Gemini is not on this machine, so Orion gives it no private notes, journal entries or private files, and asks before sending it a message that looks private. Routing uses the keyword rules rather than asking Gemini to judge each message.
- **A pasted key** with a stray space, line break or invisible character still works: Orion drops them.

### Check

```sh
orion doctor
orion chat
```

`orion doctor` checks each part in turn and offers to fix what it can (see [Troubleshooting](operating.md#troubleshooting)).

The header names the models it found. With neither, it says what is missing:

```
Error: no Claude credentials: set ANTHROPIC_API_KEY (from console.anthropic.com), … Or run a local model with `orion chat --local`
asking http://127.0.0.1:11434/v1 for its models: cannot connect to http://127.0.0.1:11434/v1: is the server running?
```

## 5. Optional parts

Each part is independent. Install what you'll use.

### The sandbox

Programs the AI starts with `proc_run` always run confined. With bubblewrap installed the confinement is strongest (no network, no GPU, private paths gone). Without it, Orion uses the kernel's Landlock (Linux 5.13+), or limits only.

```sh
sudo apt install bubblewrap        # dnf / zypper / pacman: bubblewrap
orion tools | tail -1              # which sandbox is in use
```

If it says Landlock although bubblewrap is installed, bubblewrap couldn't create its namespaces here; see [Troubleshooting](operating.md#troubleshooting). Set `[sandbox] require = true` to refuse to run programs with limits only.

### Voice

Voice needs three things on this machine: a microphone recorder, speech to text, and text to speech. `/voice` in the chat shows what Orion found.

| Part | Install | Orion uses |
|---|---|---|
| Microphone | `pulseaudio-utils` (Arch: `libpulse`) for `parecord`, or `alsa-utils` for `arecord`, or `sox` or `ffmpeg` | the first one it finds, or `[voice] record_command` |
| Speech to text | an OpenAI-compatible transcription server on `http://127.0.0.1:8000/v1`, such as [speaches](https://github.com/speaches-ai/speaches), LocalAI or whisper.cpp's server; or whisper.cpp's `whisper-cli` as a command | `[voice] transcribe_url` and `transcribe_model`, or `transcribe_command` |
| Text to speech | `espeak-ng` (or `espeak`, `spd-say`); or [piper](https://github.com/rhasspy/piper); or a speech server such as Kokoro-FastAPI | the first one it finds, or `speak_command` / `speak_url` |

With whisper.cpp and no server, for example:

```toml
[voice]
transcribe_command = "whisper-cli -m ~/models/ggml-base.en.bin -nt -np -f {wav}"
```

Speaker verification, so only enrolled voices can wake Orion:

```sh
orion voice model install     # the neural speaker model, about 16 MB from PyPI, once
orion voice enroll            # you: six short phrases
orion voice enroll --name ana # anyone else
orion voice test
```

On a machine without internet, download the Resemblyzer 0.1.4 wheel elsewhere and use `orion voice model install --from <file>`.

A wake-word detector, so Orion transcribes only after it hears its name. On a laptop processor this is faster and more reliable than transcribing everything:

```sh
install -Dm 755 contrib/orion-wake ~/.local/bin/orion-wake
pip install --user vosk                 # or: openwakeword onnxruntime (see the README)
orion-wake --verbose                    # say "Orion"; Ctrl-C to stop
```

Then set it under `[voice]` in `~/.config/orion/orion.toml`. If the file already has a `[voice]` section, this adds the line there:

```sh
sed -i '/^\[voice\]/a wake_command = "~/.local/bin/orion-wake"' ~/.config/orion/orion.toml
```

Without a `[voice]` section, add one at the end: `printf '\n[voice]\nwake_command = "~/.local/bin/orion-wake"\n' >> ~/.config/orion/orion.toml`.

If noise keeps Orion listening after you stop talking, run `orion voice noise` and add the settings it prints under `[voice]`.

### The graphical shell

`orion shell` needs only a browser on the same machine. To make Orion the whole desktop, install the `cage` kiosk compositor and Chromium (or Firefox), then run `contrib/orion-kiosk` as the session:

```sh
sudo apt install cage chromium-browser     # Fedora, openSUSE, Arch: cage chromium
sudo install -m 755 contrib/orion-kiosk /usr/local/bin/orion-kiosk
cage -- orion-kiosk                        # from a TTY login or a display manager session
```

`orion-kiosk` runs `orion` from the `PATH`, so the session needs `~/.cargo/bin` on it (or the binary in `/usr/local/bin`).

### Network, firewall, disks and GPUs

| Tools | Need |
|---|---|
| `net_connections`, `wifi_scan`, `wifi_connect`, `net_connection` | NetworkManager (`nmcli`) |
| `fw_status`, `fw_rule`, `fw_enable` | ufw or firewalld |
| `disk_mount`, `disk_unmount` | `mount`, or udisks2 for desktop-style mounts without root |
| `gpu_info` on NVIDIA | the NVIDIA driver's `nvidia-smi`. AMD and Intel need nothing extra. |
| `model_load`, `model_pull` | Ollama |
| `audio_status`, `audio_set` | PipeWire (with pipewire-pulse) or PulseAudio, and `pactl` 16 or newer |
| `display_status`, `display_set` | `wlr-randr` (sway, Hyprland and other wlroots compositors), `kscreen-doctor` (KDE) or `xrandr` (X11); brightness: `brightnessctl`, or nothing extra under logind |
| `power_set` | power-profiles-daemon, or tuned-ppd (`powerprofilesctl`) |
| `bt_status`, `bt_device` | BlueZ (`bluetoothctl`) |
| `app_open` without an app | `xdg-open` (xdg-utils) |
| `clock_set` | systemd (`timedatectl`, `localectl`) |
| `disk_health`, the disk-health check | smartmontools (`smartctl`), and root: the privileged helper or sudo |
| `sensor_status`, the temperature check | the kernel's sensor drivers (`sensors-detect` from lm-sensors finds them); NVIDIA's driver needs `nvidia-smi` |
| The error and login checks | membership of `systemd-journal` (or `adm`/`wheel`) to read the system journal |

Tools whose program is missing report that when called. The others still work.

## 6. Root access for the machine tools

Installing packages and updates, controlling system services, changing the network or firewall, mounting disks and pinning GPUs need root. Orion never asks for your password itself. The recommended way is **the privileged helper**; sudo and pkexec also work.

### The privileged helper (recommended)

`orion helper` is a small root service that does Orion's root work and nothing else. Orion sends it each command on a Unix socket that only root and the `orion` group can open. The helper runs the command only if it is one Orion's own tools build, and checks it again on its own side, against its own policy:

- **Packages and updates:** only the package manager commands Orion makes, on plain package names (no files, no options that run commands), and only if the helper's own dry run, as root, removes no protected or essential package. Rollbacks may name old versions (`name=version`) or pacman's cached package files, and nothing else.
- **Snapshots:** only the `snapper` commands Orion makes (a pre or post snapshot described "orion …", or undoing a pair), and only on the snapper configuration in the helper's own policy (`[packages] snapper_config`, default `root`).
- **Services:** `systemctl` start, stop, restart, reload, enable and disable of units that aren't protected. That includes the units that give a root shell on a console, such as `debug-shell.service`.
- **GPU pins:** only Orion's `50-orion-gpu.conf` drop-in, holding only the GPU variables. A drop-in with `ExecStart=` would run anything as root.
- **Disks:** mounts only under the mount roots, always `nosuid,nodev`, of disks the system doesn't run from; unmounts of anything but the system's own mounts.
- **Audit checkpoints:** the helper appends the asker's audit-log checkpoints (a line count and a hash, at most one a minute) to `/var/lib/orion/audit-anchors/UID.jsonl`; no program runs. `orion audit --verify` checks them.
- **Disk health:** only `smartctl --json=c -a --nocheck=standby` on a whole disk listed in /sys/block: reading SMART data, never changing a setting or starting a self-test.
- **Clock and language:** `timedatectl` and `localectl` setting a time zone that exists in `/usr/share/zoneinfo`, network time on or off, a locale generated on the machine, or an XKB keyboard layout. Never the time itself.
- **Network:** NetworkManager's Wi-Fi and connection commands as Orion makes them (no VPN imports, which can run scripts), and firewall rules that keep SSH open.

Its protected lists come from `/etc/orion/helper.toml`, which only root may change, so code running as you can't loosen them. Everything else is refused, and every command it ran or refused is logged with who asked. Compared with a sudo rule, code running as you gets Orion's operations, not a root shell.

With a release package or a system-wide `install.sh`, the binary, the units and the `orion` group are already there. Join the group and start it:

```sh
sudo usermod -aG orion "$USER"                                 # then log out and back in
sudo systemctl enable --now orion-helper.socket
```

Built from source, set it up once, as root:

```sh
sudo install -m 755 ~/.cargo/bin/orion /usr/local/bin/orion   # the helper runs from /usr/local/bin
sudo groupadd --system orion
sudo usermod -aG orion "$USER"                                 # then log out and back in
sudo cp contrib/orion-helper.socket contrib/orion-helper.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now orion-helper.socket
```

Check it:

```sh
orion tools | tail -1
# root access for the machine tools: the Orion helper at /run/orion/helper.sock
journalctl -u orion-helper -f       # what it runs and refuses, and who asked
```

With `[admin] elevate = "auto"` (the default), Orion uses the helper whenever its socket exists, so nothing else needs changing. Set `elevate = "helper"` to use only the helper, never sudo.

To give the helper its own protected lists or another group, copy the sections you want to change from `orion.example.toml` (`[packages]`, `[services]`, `[disks]`, `[helper]`) into `/etc/orion/helper.toml`. A list there replaces the default list, so copy the whole list and add to it. The file must belong to root and be writable only by root, or the helper won't start.

`tests/helper/run.sh` (as root) checks all of this end to end: a normal user installing, rolling back and removing a package through the helper, with snapshots of a scratch btrfs volume, and the helper refusing a shell, an apt hook, a protected package, a unit that runs a command, a console root shell, a snapper command Orion never makes, a weak mount and an outsider.

### Other ways

`[admin] elevate` chooses:

| `elevate` | How |
|---|---|
| `"auto"` (default) | Nothing extra when Orion runs as root. Otherwise system services and NetworkManager are tried directly first (polkit may allow them), then the helper if its socket exists, else `sudo -n`. |
| `"helper"` | Only the helper. |
| `"sudo"` | Always `sudo -n`: it works only with a passwordless sudoers rule, and fails at once without one. |
| `"pkexec"` | polkit's password dialog, for a desktop session. |
| `"none"` | Never. Those tools then fail unless Orion runs as root. |

**With sudo**, add a sudoers rule for the programs you want the AI to be able to run, with `sudo visudo -f /etc/sudoers.d/orion`. Replace `you` with your login name and keep only the lines you need:

```
# Packages and updates (Debian/Ubuntu: Orion runs apt-get through env)
you ALL=(root) NOPASSWD: /usr/bin/env DEBIAN_FRONTEND=noninteractive apt-get *
# Fedora: /usr/bin/dnf *    openSUSE: /usr/bin/zypper *    Arch: /usr/bin/pacman *
# System services, and the GPU pins gpu_assign writes as systemd drop-ins
you ALL=(root) NOPASSWD: /usr/bin/systemctl *
you ALL=(root) NOPASSWD: /usr/bin/mkdir -p -- /etc/systemd/system/*, /usr/bin/tee -- /etc/systemd/system/*/50-orion-gpu.conf, /usr/bin/rm -f -- /etc/systemd/system/*/50-orion-gpu.conf
# Network and firewall
you ALL=(root) NOPASSWD: /usr/bin/nmcli *, /usr/sbin/ufw *
# Disks (without udisks2)
you ALL=(root) NOPASSWD: /usr/bin/mkdir -p -- /mnt/*, /usr/bin/mkdir -p -- /media/*, /usr/bin/mount -o *, /usr/bin/umount -- *, /usr/bin/eject -- *
```

Check the paths on your system with `command -v apt-get systemctl nmcli ufw`.

**Know what this grants.** Orion checks every request against its policy and protected lists before it runs anything. But a passwordless rule for a package manager or `systemctl` lets *anything running as your user* become root, not only Orion: a package manager can run any script, and `systemctl` can start any unit. That is why the helper is recommended.

**Running the daemon with the hardened service unit?** `contrib/orion.service` sets `NoNewPrivileges=yes`, which stops sudo and pkexec from working inside it. The helper isn't affected: the daemon only connects to its socket. With sudo, remove that line; see [step 7](#7-run-it-as-a-service).

## 7. Run it as a service

Three user services come in `contrib/`. Each reads variables such as `ANTHROPIC_API_KEY` from `~/.config/orion/env`.

```sh
mkdir -p ~/.config/systemd/user
```

**The daemon**, so every AI agent on the machine shares one policy, audit log and sandbox. systemd holds the socket and starts the daemon on the first connection:

```sh
cp contrib/orion.socket contrib/orion.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now orion.socket
orion status
```

**Headless voice** (`orion listen`): a voice assistant with no screen.

```sh
cp contrib/orion-voice.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now orion-voice.service
journalctl --user -u orion-voice -f
```

**The graphical shell** in the background. Its address, with this run's token, is in `~/.local/state/orion/shell-url`:

```sh
cp contrib/orion-shell.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now orion-shell.service
cat ~/.local/state/orion/shell-url
```

**Approvals for AI agents by notification** (`orion approvals --notify`): when an agent connected through the daemon calls a tool that needs approval, a notification with Approve and Decline appears. Needs `notify-send` (libnotify).

```sh
cp contrib/orion-approvals.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now orion-approvals.service
```

User services stop when you log out. To keep them running without a login session (on a server, or for headless voice):

```sh
sudo loginctl enable-linger "$USER"
```

## 8. Connect other AI agents (MCP)

Any MCP client can use Orion's tools by launching `orion serve`. For Claude Code:

```sh
claude mcp add orion -- ~/.cargo/bin/orion serve
```

In a client's JSON configuration:

```json
{ "mcpServers": { "orion": { "command": "/home/you/.cargo/bin/orion", "args": ["serve"] } } }
```

With no human at the keyboard, `ask` means no: an MCP client is offered only the tools the policy sets to `allow`. When the daemon runs, `orion serve` connects to it, so every client shares it, and the audit log names each one.

## Upgrading

A release package upgrades like any other: install the new one the same way (`sudo apt install ./orion_…deb`, `sudo dnf install ./orion-….rpm`, `makepkg -si` with the new PKGBUILD). With the install script, run it again. Installed with `bootstrap.sh`, run `sh ~/Orion/packaging/bootstrap.sh` again: it pulls, rebuilds and restarts the helper and the daemon itself. Then restart what runs:

```sh
systemctl --user restart orion.service orion-voice.service orion-shell.service 2>/dev/null
sudo systemctl restart orion-helper.service 2>/dev/null
```

Built from source:

```sh
cd Orion
git pull
cargo install --path . --force
systemctl --user restart orion.service orion-voice.service orion-shell.service 2>/dev/null
```

The helper runs its own copy, so with the helper set up (step 6), update that too:

```sh
sudo install -m 755 ~/.cargo/bin/orion /usr/local/bin/orion
sudo systemctl restart orion-helper.service
```

`orion tools` reminds you when the helper's copy is older than the `orion` you run.

Your policy, conversations, memory, voiceprints and audit log are kept. A new version may refuse a setting it no longer knows; the error names it.

## Uninstalling

A release package: `sudo apt remove orion`, `sudo dnf remove orion`, `sudo zypper remove orion` or `sudo pacman -R orion-bin`. The install script: run it again with `--uninstall` (with `sudo` if you installed it that way). Built from source:

```sh
systemctl --user disable --now orion.socket orion.service orion-voice.service orion-shell.service 2>/dev/null
rm -f ~/.config/systemd/user/orion*.service ~/.config/systemd/user/orion.socket
cargo uninstall orion                       # or: sudo rm /usr/local/bin/orion
```

That leaves your data. To remove it too:

| Path | What |
|---|---|
| `~/.config/orion/` | the policy, the `env` file, `voice-tuning.toml` and `tuning.toml` (settings Orion tuned with your approval) |
| `~/.local/state/orion/` | conversations, memory, voiceprints, the audit log, chat history, the shell's address, the package history, the last unattended update, `voice.json` (how recent spoken requests ended, for voice tuning) and `tuning.json` (rate limits and transcription times, for settings tuning) |
| `~/.local/share/orion/` | the neural speaker model |
| `~/orion-workspace/` | files the AI wrote (the default write root) |
| `/etc/sudoers.d/orion` | the sudoers rule, if you made one |
| `/etc/systemd/system/orion-helper.*`, `/etc/orion/`, `/usr/local/bin/orion` | the privileged helper: `sudo systemctl disable --now orion-helper.socket orion-helper.service` first, then `sudo groupdel orion` |
| `/etc/systemd/system/*.d/50-orion-gpu.conf` | GPU pins made by `gpu_assign`; remove them with `gpu_assign` and an empty list first |

Packages Orion installed, firewall rules it added (commented "added by Orion" in ufw) and Wi-Fi networks it joined stay as they are.

## Making a release

1. Set the new version in `Cargo.toml` (and run `cargo build` so `Cargo.lock` follows), and commit.
2. Tag it and push the tag: `git tag v0.2.0 && git push origin v0.2.0`. Or, where you can't push a tag, run the Release workflow by hand (Actions → Release → Run workflow) on the branch with **publish** checked: once the tests pass, it creates the tag `v<version>` on that commit along with the release. It refuses a version that is released already.

`.github/workflows/release.yml` then builds the binaries for x86_64 and aarch64 against glibc 2.31, makes the tarballs, `.deb`, `.rpm`, PKGBUILD, `install.sh` and `SHA256SUMS`, installs them in clean containers (Debian 11, Ubuntu, Rocky Linux 9, Fedora, openSUSE, Arch, and arm64 under emulation), runs the distro tests against the release binary, and only then publishes the GitHub release. A tag that doesn't match the version in `Cargo.toml` stops the build. Every push runs the same build and package tests without publishing, and keeps the files as workflow artifacts.

To build them on your own machine (needs zig, `cargo-zigbuild`, dpkg-deb and rpmbuild):

```sh
cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.31
packaging/build.sh 0.1.0 x86_64 target/x86_64-unknown-linux-gnu/release/orion dist
packaging/assemble.sh dist 0.1.0       # after building every architecture into dist
packaging/test.sh dist 0.1.0           # install and check them in Docker
```
