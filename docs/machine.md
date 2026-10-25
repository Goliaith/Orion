# The machine

What Orion's tools do, and the rules each one follows. You don't need to call these yourself: ask in the chat and the AI picks them. [All tools](tools.md) lists them in one table, and [Operating Orion](operating.md#looking-after-the-machine) has `orion call` examples.

- [Watching the machine](#watching-the-machine)
- [Flatpak apps](#flatpak-apps)
- [Packages](#packages)
- [System updates](#system-updates)
- [Unattended updates](#unattended-updates)
- [Services](#services)
- [Timers](#timers)
- [Files](#files)
- [Processes](#processes)
- [The system journal](#the-system-journal)
- [The desktop](#the-desktop)
- [Network](#network)
- [Disks and mounts](#disks-and-mounts)
- [GPUs](#gpus)
- [Local models and VRAM](#local-models-and-vram)
- [The background service (daemon)](#the-background-service-daemon)
- [Approvals for other AI agents](#approvals-for-other-ai-agents)
- [The privileged helper](#the-privileged-helper)

## Watching the machine

The background service checks the machine every 15 minutes and tells you when something needs attention:

| Check | Needs attention when |
|---|---|
| Services | a system service, or one of yours, has failed |
| Disks | a filesystem is 90% full (`disk_percent`) |
| Updates | security updates are waiting (checked every 6 hours) |
| Reboot | the kernel or core libraries were replaced |
| Memory | under 5% is free (`memory_percent`) |
| Disk health | a drive's SMART data says it's failing or wearing out (every 6 hours; needs smartmontools and the helper) |
| Temperatures | a CPU, GPU, drive or board is near its limit |
| Battery | it holds under 70% of its charge when new (`battery_health_percent`) |
| Errors | one program logs 50 errors in an interval (`journal_errors`) |
| Hardware errors | the kernel reports a CPU, memory, disk or GPU error, overheating, or running out of memory |
| Logins | 10 failed logins in an hour (`failed_logins`) |
| Settings | a setting keeps getting in the way ([self-tuning](setup.md#self-tuning)) |
| Voice | listening keeps going wrong ([voice tuning](voice.md#voice-tuning)) |

How you hear about it:

- **A desktop notification** the first time each problem appears, and again if it comes back after clearing.
- **`orion status`** lists each problem with what to do about it.
- **The AI is told** at the start of each conversation, mentions it once when it fits, and offers to help. It doesn't act unasked.
- **The chat and the shell** show the findings in their machine panel.

Settings are under `[watch]`: the interval, the thresholds, `notify`, and `ignore` for findings you don't care about, such as `["disk:/mnt/archive", "service:backup.service", "temp:*"]`.

Reading the system journal (for the error and login checks) needs the `systemd-journal` group: `sudo usermod -aG systemd-journal "$USER"`, then log in again.

## Flatpak apps

Many desktop apps (Discord, Spotify, Steam, Zoom) come from Flathub. Ask "install Discord" and the AI uses:

- `flatpak_search` to find the app id (`com.discordapp.Discord`),
- `flatpak_install` to install it,
- `app_open` to start it, and `flatpak_remove` to uninstall it.

How it works:

- **System-wide by default,** as Discover installs on KDE. Orion runs `flatpak` as you first, which polkit allows for desktop admins on Fedora, and as root through the privileged helper if polkit says no. With `scope: "user"` it installs only for you, with no root.
- **Under the background service** (`orion.service`), system-wide installs go straight to the helper, because the service's hardening blocks the setuid helper Flatpak needs. Without the helper they are refused before you are asked.
- **Only from `[flatpak] remotes`** (default `["flathub"]`). The app and the remote are checked before you are asked, and the helper runs only Orion's own install and uninstall commands.
- **Not through `proc_run`.** Sandboxed programs have no network, so `flatpak install` (or `dnf install`) through `proc_run` is refused with a pointer to the right tool.
- **Without asking:** the Everyday and Hands-off [trust levels](safety.md#trust-levels) set `[flatpak] allowed = ["*"]`. To allow only some, list them: `["com.discordapp.Discord"]`.

Fedora KDE usually has Flathub already. If it doesn't, Orion says so, and you can add it in a terminal: `flatpak remote-add --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo`.

## Packages

`pkg_list`, `pkg_search`, `pkg_info`, `pkg_install` and `pkg_remove` drive apt, dnf, zypper or pacman, found automatically (or set `[packages] manager`).

| Package manager | Distributions | Tested on |
|---|---|---|
| apt | Debian, Ubuntu, Mint, Pop!_OS | Ubuntu 24.04, Debian |
| dnf (dnf4 and dnf5) | Fedora, RHEL 8+, Rocky, Alma | Fedora 44 |
| zypper | openSUSE, SLES | Tumbleweed |
| pacman | Arch, Manjaro, EndeavourOS | Arch |

- **Dry run first.** Every change is tried with the package manager's own dry run before you are asked, so unknown packages fail first, and you see every package that would change.
- **Removals are guarded.** A removal is refused if it would take away an essential package or one in `[packages] protected` (the kernel, systemd, sudo, ssh, glibc, the package manager, bubblewrap, …), even as a dependency.
- **`[packages] allowed`** lists packages that install without asking, with wildcards: `["python3-*"]`.
- **Root:** installs run as root through the [helper](#the-privileged-helper), or `sudo -n`.
- **Undo:** every change is recorded and snapshotted where snapper is set up ([rollback](safety.md#snapshots-and-rollback)).

## System updates

- **`update_check`** refreshes the package lists and lists every update, security fixes first. Security fixes come from the repositories' own labels (Arch has none).
- **`update_apply`** installs everything, only security fixes (`security_only`), or only named packages.

Rules:

- **Nothing is removed unless you say so.** If an update would remove or replace a package, it's held back (apt) or refused with what it would remove (dnf, zypper, pacman). `full: true` allows it.
- **Protected packages are never removed,** even with `full`.
- **Config files you changed are kept,** and nothing waits on a prompt.
- **After an update,** Orion says what still runs old code: services to restart, other programs, and whether to reboot and why. Orion never reboots on its own.
- **Without asking:** `[packages] updates = "security"` (security fixes), `"all"` (any update that removes nothing) or `"full"` (any update).
- **Undo:** every update is recorded and snapshotted ([rollback](safety.md#snapshots-and-rollback)).

## Unattended updates

To have the background service install updates once a day:

```toml
[packages]
updates = "security"        # security fixes need no approval
automatic = "security"      # install them daily ("all" needs updates = "all")
automatic_at = "04:00"      # local time; caught up if the machine was off
automatic_on_battery = false
```

- Each run goes through the policy like any other caller, never removes packages, and is snapshotted.
- You get a desktop notification when it installed something. A failed run shows under what needs attention and tries again the next day.
- `orion status` shows the setting and the last run.

[Operating Orion](operating.md#unattended-updates) also shows a root timer that works without the background service.

## Services

`svc_list`, `svc_status` and `svc_control` work on systemd units, the machine's or your own (`"scope": "user"`).

- **`svc_status` explains failures:** state, last result, restarts, memory, CPU and the last log lines.
- **Protected units can't be touched:** ssh, networking, the display manager, `systemd-*`, D-Bus, polkit, logins, every `.target`, and Orion itself. Edit `[services] protected` to change the list.
- **`[services] allowed`** lists units the AI may control without asking, like `["ollama.service"]`.
- **Root:** system units are tried directly first (polkit may allow it), then through the helper or `sudo -n`.

## Timers

`timer_list` lists every systemd timer. `timer_create` schedules a tool call (`"daily"`, `"Mon..Fri 08:30"`, any systemd schedule), and `timer_remove` removes one.

- **A scheduled call is an ordinary call:** it goes through the policy, the sandbox and the audit log (as `"caller": "timer <name>"`).
- **Nobody is there to approve,** so only pre-approved calls can be scheduled. `timer_create` says what to pre-approve.
- **They are your user timers** (`~/.config/systemd/user/orion-timer-<name>.*`), running while you're logged in, or always with `loginctl enable-linger`.

## Files

`fs_list`, `fs_read` and `fs_find` look inside the readable roots. `fs_write`, `fs_mkdir`, `fs_copy`, `fs_move` and `fs_delete` change things only inside the writable roots (default `~/orion-workspace`).

- **Links are never followed out** of a root.
- **Nothing is replaced unless asked** (`overwrite`, and never a folder).
- **Deleting goes to the Trash,** the one your file manager shows. `permanent: true` deletes for good.
- **`fs_find`** matches names (with `*` and `?`) or text inside small files.

## Processes

`proc_list` shows processes. `proc_signal` stops, interrupts, pauses or resumes one, by pid or name.

- **Never stopped:** PID 1, kernel threads, Orion, protected services' processes, and `[proc] protected` (systemd, sshd, D-Bus, NetworkManager, the display server, …).
- **A name must be unambiguous,** or `all: true` acts on every match.
- **A process a service runs may come back;** stop the service instead.

`proc_run` runs one program in the [sandbox](safety.md#the-sandbox). `[proc] allowed_commands` lists programs that run without asking.

## The system journal

`journal_read` reads systemd's journal: by unit, time (`"1h ago"`, `"today"`), priority (`"err"`), boot (`-1` is the one before), kernel messages, or text. Without the `systemd-journal` group, only your own programs' entries are shown.

## The desktop

The desktop tools act on the session of whoever is logged in.

- **Sound** through `pactl` (PipeWire or PulseAudio). Devices by name or part of it ("HDMI", "headphones"), volume up to 150%.
- **Screens** with `kscreen-doctor` on KDE Plasma, `wlr-randr` on sway and Hyprland, or `xrandr` on X11. On GNOME Wayland, only brightness. The last screen that's on is never turned off. Brightness is the laptop panel's, never below 5%.
- **Power:** battery charge, time left and health; power profiles (power-profiles-daemon or tuned-ppd); lock, suspend, reboot and power off through logind.
- **Bluetooth** through `bluetoothctl`. Pairing suits headphones, speakers and mice; keyboards that show a code aren't supported yet.
- **Apps** from their desktop entries, Flatpak and Snap included. An app starts in its own systemd unit, so it outlives Orion. Files must be in the readable roots, links must be http, https or mailto, and programs are never opened as files.
- **The clock and language:** time zone, network time, system locale and keyboard layout, through `timedatectl` and `localectl`. KDE keeps its own per-person settings too.

`[desktop] approved` lists what needs no approval: `volume`, `brightness` and `lock` by default; also `displays`, `power_profile`, `suspend`, `hibernate`, `reboot`, `poweroff`, `bluetooth`, `clock` and `open`. `[desktop] apps` pre-approves apps by id, with wildcards.

If a desktop tool says no session is running, check that `systemctl --user show-environment` lists `WAYLAND_DISPLAY` or `DISPLAY`. KDE sets this up.

## Network

- **`net_info`:** interfaces, addresses, routes, DNS, and listening ports with the program behind each.
- **Wi-Fi:** `wifi_scan` lists networks; `wifi_connect` joins one. The password is never shown, logged or put on a command line, and a message containing "password" goes to the local model.
- **Connections:** `net_connections` lists them; `net_connection` brings one up or down, forgets it, or toggles auto-connect.
- **Firewall** (firewalld on Fedora, or ufw): `fw_status`, `fw_rule` (allow or deny a port, optionally from one network) and `fw_enable`.
- **SSH can't be cut off.** Orion refuses to take down a connection an SSH session runs over, deny the SSH port, or turn on the firewall without SSH allowed.

## Disks and mounts

- **`disk_list`:** every disk and partition, its size, filesystem, label, where it's mounted and how full.
- **`disk_usage`:** what is filling a folder, biggest first.
- **`disk_mount`** mounts a drive by path, `LABEL=` or `UUID=`, always `nosuid,nodev`. Refused: encrypted volumes (unlock first), swap, RAID or LVM members, and the system's own disk.
- **`disk_unmount`** unmounts, and `eject: true` makes the drive safe to unplug. If it's busy, it names the programs using it.
- **Protected:** `/`, `/boot`, `/usr`, `/var`, `/home`, `/etc` and the like can't be unmounted.
- **Not offered, on purpose:** formatting, partitioning, and editing `/etc/fstab`.

## GPUs

`gpu_info` recognizes every GPU, NVIDIA, AMD and Intel: its name, memory, load, temperature and power, its compute stack (CUDA, ROCm, oneAPI, Vulkan), and what runs on it, down to the model an inference server is serving.

| | NVIDIA | AMD | Intel |
|---|---|---|---|
| Name | `nvidia-smi` | the driver, else the PCI ID database | the PCI ID database |
| Memory, load, temperature, power | `nvidia-smi` | amdgpu's sysfs files | integrated: shares system memory |
| Compute | CUDA | ROCm | oneAPI Level Zero |
| What runs on it | `nvidia-smi` | the kernel's DRM fdinfo | the kernel's DRM fdinfo |

- **Each GPU gets a number by PCI address,** so it stays the same across boots.
- **Inference servers are traced to their model:** Ollama's runners by its manifests ("qwen3:32b"), llama.cpp by its model file, vLLM by its model name.
- **`gpu_assign`** pins an inference server (a systemd service) to chosen GPUs, with a systemd drop-in, then restarts it: `gpu_assign {"unit": "ollama", "gpus": [1]}`. An empty list removes the pin.

## Local models and VRAM

`model_status` finds inference servers on this machine (Ollama, llama.cpp, vLLM, LM Studio), their models, what's loaded, and each GPU's free memory.

- **`model_load`** checks a model fits before you are asked (about 1.15 × its size plus 512 MB), and says which GPU it will land on. With `evict: true` it first unloads the least recently used models.
- **The budget** is each GPU's memory less `[models] vram_reserve_mb` (1 GB) for the desktop, less what's in use.
- **`keep_alive`** sets how long a model stays loaded (default `30m`).
- **Loading and pulling work on Ollama.** Other servers serve one fixed model; manage them as services.

## The background service (daemon)

`orion daemon` runs Orion in the background, so every AI agent on the machine shares one policy, one audit log and one sandbox. `bootstrap.sh` sets it up as a user service. By hand:

```sh
cp contrib/orion.socket contrib/orion.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now orion.socket
orion status
```

- **It watches the machine** ([above](#watching-the-machine)) and runs [unattended updates](#unattended-updates).
- **Policy edits apply within two seconds.** A broken edit is reported in `orion status`, and the old policy stays in force.
- **The chat, the shell and `orion listen` use it** when it runs the same version and policy. Their approvals still appear in the chat.
- **It listens on a private socket** only you can reach, and checks every connection.
- **It reads your key** from `~/.config/orion/env`, and is hardened by systemd (memory and task limits, `NoNewPrivileges`, …).
- **Its log:** `journalctl --user -u orion -f`.

## Approvals for other AI agents

Through the daemon, other AI agents (Claude Code, or any MCP client running `orion serve`) can use the tools that ask, and a person approves each call:

```sh
orion approvals              # answer in this terminal
orion approvals --notify     # desktop notifications with Approve and Decline
orion approvals --list       # what's waiting
```

The shell shows them as cards too. The first answer counts. With nobody watching, the call is refused at once; with nobody answering in 2 minutes, it counts as no. `[daemon] approvals = false` offers agents only the tools that run without asking.

## The privileged helper

`orion helper` does Orion's root work, so Orion needs no sudo rule. It's a small root service that only root and the `orion` group can reach. It runs a command only if it's one of Orion's own, checked again against its own policy, `/etc/orion/helper.toml`, which only root can change:

- package manager commands on plain package names, whose dry run removes nothing protected;
- `systemctl` actions on units that aren't protected;
- Flatpak installs and uninstalls from its own list of remotes;
- GPU pins, `nosuid,nodev` mounts, NetworkManager's Wi-Fi commands, and firewall rules that keep SSH open;
- snapper snapshots and rollbacks, SMART readings, time zone and locale changes;
- and only for the logins in its `[helper] root_users` (none by default), [root work](safety.md#root-work): `root_run` and `sys_edit`.

Everything else is refused. Each command is logged with who asked: `journalctl -u orion-helper`. `bootstrap.sh` sets the helper up; [Installing Orion](install.md#the-privileged-helper-recommended) shows how by hand.
