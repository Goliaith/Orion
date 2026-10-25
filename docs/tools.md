# All tools

The AI works on the machine only through these tools. Each has a **risk**:

- **read** looks at the machine and changes nothing.
- **write** changes something.
- **execute** runs a program or a package manager.

The **default** is what happens with no policy file:

- **allow** runs without asking.
- **ask** stops and asks you first.
- **always asks** asks every time, whatever your policy or [trust level](safety.md#trust-levels) says.

Your trust level and policy file change these defaults. To see what your own setup decides for each tool, run:

```sh
orion tools
```

With nobody there to answer (`orion serve` without the daemon, or a timer), "ask" means no.

## The machine

| Tool | Risk | Default | What it does |
|---|---|---|---|
| `system_info` | read | allow | Hostname, OS, kernel, CPU, memory, load, uptime |
| `system_health` | read | allow | What needs attention: failed services, full or failing disks, security updates, a due reboot, low memory, heat, battery wear, journal errors, failed logins, voice trouble, settings that keep getting in the way |
| `disk_health` | read | allow | Each drive's SMART health: good, warning or failing and why, wear, temperature, hours of use |
| `sensor_status` | read | allow | Temperatures (CPU, GPU, drives, board) with their limits, and fan speeds |
| `journal_read` | read | allow | The system journal, by unit, time, priority, boot or text |

## Settings and voice tuning

| Tool | Risk | Default | What it does |
|---|---|---|---|
| `settings_check` | read | allow | Settings that keep getting in the way (rate limits, slow speech to text, tools you approve every time), with the fix for each and why |
| `settings_tune` | write | always asks | Make a change `settings_check` suggests, or undo one ([self-tuning](setup.md#self-tuning)) |
| `voice_check` | read | allow | How listening is going, and suggested changes with their reasons |
| `voice_tune` | write | ask | Change how Orion listens (noise margin, speech level, pause, time limit, wake-word spellings), and nothing else |

## Files

| Tool | Risk | Default | What it does |
|---|---|---|---|
| `fs_list` | read | allow | List a folder inside the readable roots |
| `fs_read` | read | allow | Read a file, or part of one |
| `fs_find` | read | allow | Find files by name (wildcards) or by text inside them |
| `fs_write` | write | ask | Write or append to a file inside the writable roots |
| `fs_mkdir` | write | ask | Create a folder |
| `fs_copy` | write | ask | Copy a file or folder |
| `fs_move` | write | ask | Move or rename a file or folder |
| `fs_delete` | write | ask | Delete to the Trash (or for good) |
| `disk_usage` | read | allow | What is using the space in a folder, biggest first |

## Programs and root work

| Tool | Risk | Default | What it does |
|---|---|---|---|
| `proc_list` | read | allow | Processes, with filtering and sorting by memory |
| `proc_run` | execute | ask | Run one program (no shell) in the [sandbox](safety.md#the-sandbox). With `"sandbox": false`, run it as you outside the sandbox (always asks) |
| `proc_signal` | write | ask | Stop, interrupt, pause or resume a process; never the ones the system needs |
| `dev_shell` | execute | asks to open a shell | [Developer mode](safety.md#developer-mode) only: run a command line in a persistent bash in `~/orion-dev`. Opening a shell asks you (an unsandboxed one, every time); its commands then run without asking |
| `dev_shell_read` | read | allow | Follow a dev shell's command that is still running: what it printed since, and how it ended |
| `dev_shell_close` | write | allow | Close a dev shell and stop everything it runs |
| `dev_history` | read | allow | The checkpoints of `~/orion-dev`, one before each dev shell command |
| `dev_rollback` | write | ask | Put `~/orion-dev` back as a checkpoint found it; undoable |
| `root_run` | execute | always asks | Run one program as root through the helper, for work no other tool covers; snapshotted ([root work](safety.md#root-work)) |
| `sys_edit` | write | always asks | Write a system file (in `/etc`, say) as root, approved as a diff; snapshotted ([root work](safety.md#root-work)) |

## Services and timers

| Tool | Risk | Default | What it does |
|---|---|---|---|
| `svc_list` | read | allow | systemd units: running, failed or stopped, and enabled at boot |
| `svc_status` | read | allow | One unit's state, last result, memory, CPU, restarts and recent log |
| `svc_control` | write | ask | Start, stop, restart, reload, enable or disable a unit (critical ones are protected) |
| `timer_list` | read | allow | systemd timers: what each runs, when it ran and runs next |
| `timer_create` | write | ask | Schedule a tool call, which runs through the policy and log when it fires |
| `timer_remove` | write | ask | Remove a timer Orion made |

## Packages, apps and updates

| Tool | Risk | Default | What it does |
|---|---|---|---|
| `pkg_list` | read | allow | Installed packages (apt, dnf, zypper or pacman) |
| `pkg_search` | read | allow | Search the repositories |
| `pkg_info` | read | allow | One package: versions, description, dependencies |
| `pkg_install` | execute | ask | Install packages, as root and outside the sandbox |
| `pkg_remove` | execute | ask | Remove packages; refused if the system needs them |
| `flatpak_search` | read | allow | Search Flathub for apps |
| `flatpak_install` | execute | ask | Install a Flatpak app, like Discord |
| `flatpak_remove` | execute | ask | Uninstall a Flatpak app |
| `update_check` | read | allow | Pending updates (security fixes and kernels marked), services to restart, reboot needed |
| `update_apply` | execute | ask | Install updates: all, security fixes only, or named packages |
| `pkg_history` | read | allow | Every package change and root work Orion did, with snapshots and rollbacks |
| `pkg_rollback` | execute | ask | Undo a change, through its snapshots or by reinstalling the old versions |
| `app_list` | read | allow | Installed desktop apps (Flatpak and Snap included) |
| `app_open` | execute | ask | Start an app, or open files and links in their default apps |

## The desktop

| Tool | Risk | Default | What it does |
|---|---|---|---|
| `audio_status` | read | allow | Speakers, headphones, HDMI, Bluetooth and microphones: volume, mute, the default |
| `audio_set` | write | allow\* | Set or nudge a volume, mute, or pick the default output or microphone |
| `display_status` | read | allow | Monitors (resolution, refresh rate, scale, position) and the panel's brightness |
| `display_set` | write | ask\* | Brightness, or a monitor's mode, scale, position, on or off; never the last screen off |
| `power_status` | read | allow | Battery charge, time left and health, the charger, the power profile |
| `power_set` | write | ask | Switch the power profile: performance, balanced, power-saver |
| `power_action` | write | ask\* | Lock, suspend, hibernate, reboot or power off |
| `bt_status` | read | allow | The Bluetooth adapter and its devices; scans for new ones |
| `bt_device` | write | ask | Turn Bluetooth on or off; pair, connect, disconnect or forget a device |
| `clock_status` | read | allow | Local time, time zone, network time, the system locale and keyboard layout |
| `clock_set` | write | ask | Set the time zone, network time, the locale or keyboard layout |

\* Changing the volume, the panel's brightness and locking the screen are pre-approved by `[desktop] approved`, because they are harmless and easy to undo. The rest of those tools ask.

## Network and disks

| Tool | Risk | Default | What it does |
|---|---|---|---|
| `net_info` | read | allow | Interfaces, addresses, routes, DNS, and listening ports with the process behind each |
| `net_connections` | read | allow | NetworkManager's devices and saved connections |
| `wifi_scan` | read | allow | Wi-Fi networks in range, strongest first |
| `wifi_connect` | write | ask | Join a Wi-Fi network (the password never reaches logs) |
| `net_connection` | write | ask | Bring a saved connection up or down, forget it, toggle auto-connect |
| `fw_status` | read | allow | The firewall (ufw or firewalld): on or off, rules |
| `fw_rule` | write | ask | Allow or deny a port; never blocks SSH |
| `fw_enable` | write | ask | Turn the firewall on or off; on only with SSH allowed |
| `disk_list` | read | allow | Disks and partitions: model, size, filesystem, mounts, how full |
| `disk_mount` | write | ask | Mount a drive under /mnt or /media |
| `disk_unmount` | write | ask | Unmount (and eject) a drive; never the system's own |

## GPUs and local models

| Tool | Risk | Default | What it does |
|---|---|---|---|
| `gpu_info` | read | allow | Every GPU (NVIDIA, AMD, Intel): memory, load, temperature, and what runs on each |
| `model_status` | read | allow | Local inference servers, their models, and the VRAM budget |
| `model_load` | write | ask | Load a model on Ollama, within the VRAM budget |
| `model_unload` | write | ask | Unload a model, freeing its VRAM |
| `model_pull` | write | ask | Download a model onto Ollama |
| `gpu_assign` | write | ask | Pin an inference server to chosen GPUs |

## Memory and the journal

These are offered only in the chat, the shell and `orion listen`. See [Memory and the journal](memory.md).

| Tool | What it does |
|---|---|
| `memory_save`, `memory_forget` | Remember or forget a fact about the person talking |
| `journal_write` | Keep a journal entry under a topic |
| `journal_decide` | Record a decision and why |
| `journal_recall` | Search the journal |

## Call a tool yourself

Every tool can be called by hand, exactly as the AI would call it, with the same rules and log:

```sh
orion call system_info
orion call fs_list '{"path": "~"}'
orion call update_apply '{"security_only": true}'
```

[Operating Orion](operating.md#looking-after-the-machine) has examples for each group of tools. [The machine](machine.md) explains how each one works.
