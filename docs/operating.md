# Operating Orion

How to use Orion day to day, keep it in check, and fix it when something goes wrong. [Installing Orion](install.md) gets it running first. Each feature has its own guide: [chat](chat.md), [voice](voice.md), [memory](memory.md), [safety](safety.md), [the machine](machine.md), [setup and doctor](setup.md) and [all tools](tools.md).

- [How Orion works](#how-orion-works)
- [Ways to use it](#ways-to-use-it)
- [Approvals](#approvals)
- [Tuning the policy](#tuning-the-policy)
- [Looking after the machine](#looking-after-the-machine)
- [The audit log](#the-audit-log)
- [Conversations and memory](#conversations-and-memory)
- [Voice](#voice)
- [The daemon](#the-daemon)
- [Files and settings](#files-and-settings)
- [Troubleshooting](#troubleshooting)
- [Security checklist](#security-checklist)

## How Orion works

A model never touches the machine directly. It asks `orion` to call a tool, such as `disk_list` or `update_apply`, and every call goes through the same four steps:

1. **The policy decides:** `allow` (run it), `ask` (stop and ask you) or `deny` (the tool isn't even offered). Decisions are per risk level (`read`, `write`, `execute`) with per-tool overrides.
2. **Hard checks run first.** Some things no approval can override: files outside the writable roots, protected services and packages, the system's own mounts, firewall rules that would lock out SSH. Those calls fail before you're asked.
3. **The tool runs.** Programs the AI asks for (`proc_run`) run in the sandbox. The machine tools run fixed commands with checked arguments, as root where they need it.
4. **The call is logged**, with its arguments (secrets blanked), the decision, your answer and the result.

Out of the box the AI can look at anything readable and change nothing without asking.

## Ways to use it

| Command | What it is | Approvals |
|---|---|---|
| `orion chat` | The chat, full screen in a terminal (`--plain` for line by line) | asked in the conversation |
| `orion shell` | The same chat as a local web page, for a browser or a kiosk | approval cards |
| `orion listen` | Voice only: "Orion, …", answered aloud | spoken, if `wake_approvals = true`; otherwise declined |
| `orion serve` | The tools over MCP, for other AI agents | through the daemon: [`orion approvals`](#approvals-for-ai-agents), a notification or the shell; without it, `ask` acts as `deny` |
| `orion call <tool> '<json>'` | One tool call by hand, as the AI would make it | asked in the terminal |

`orion chat`, `orion shell` and `orion listen` all take `--auto` (the default), `--claude` or `--local`, `--model <id>` and `--resume [id]`.

### In the chat

Type what you want in plain words: "what's using my disk?", "are there security updates?", "mount the USB drive". The AI picks the tools, and you see each call and its result as it happens.

| Command | |
|---|---|
| `@claude …`, `@local …` | send one message to that model |
| `/use auto\|claude\|local` | route automatically, or pin a model |
| `/route <text>` | show where a message would go |
| `/model [claude\|local] [id]`, `/models` | show or switch models; list the local server's |
| `/effort <level>` | Claude's effort: low, medium, high, xhigh, max |
| `/tools` | what the AI can do, and what needs your approval |
| `/status` | the daemon, the sandbox, and what needs attention |
| `/usage` | tokens used this session |
| `/new`, `/history`, `/resume <n\|id>`, `/title <text>` | conversations |
| `/memory [forget <id>\|clear]` | what the AI remembers about this person |
| `/journal [<topic>\|search <words>]` | the AI's journal |
| `/voice [speak on\|off\|test]`, `/voice wake on\|off` | voice |
| `/keys`, `/help`, `/exit` | the hotkeys; every command; leave |

Typing `orion status`, `orion tools`, `orion memory` or `orion conversations` in the chat runs the matching slash command. Commands that need a terminal (`orion doctor`, `orion setup`, …) get a note on where to run them ([details](chat.md#typing-orion-commands-in-the-chat)).

| Key | |
|---|---|
| Ctrl-C | stop the AI mid-turn (twice at the prompt: quit) |
| Shift-Tab | cycle routing: auto, claude, local |
| Ctrl-T | talk; your words land in the message box |
| Alt-W | wake-word listening on or off |
| Ctrl-O | show or hide reasoning |
| Ctrl-B | the machine panel (full screen) |
| Alt-Enter, Ctrl-J | new line |
| PgUp, PgDn, End | scroll; jump to the latest |
| Ctrl-R, ↑ ↓ | search and browse earlier messages |
| Ctrl-D | quit |

### Routing between Claude and the local model

In `auto`, each message goes to one model, and Orion shows which and why:

1. **Private messages stay local:** passwords, banking, health, keys and tokens, and anything mentioning a `private_paths` entry (`~/.ssh`, `~/.gnupg`, …). With no local model, Orion asks before sending one to Claude.
2. **Short follow-ups** ("yes, go ahead") stay with the model that answered last.
3. **The local model judges the rest** for difficulty; keyword rules decide if it can't.
4. **If the chosen model fails, the other answers**, except that private messages never go to Claude.

Claude's tool calls that touch a private path are refused, whatever the policy allows.

**Simple requests skip the models altogether.** "Volume up", "set volume to 40", "mute", "brightness 60%", "dim the screen", "status" and "open firefox" (or a link or a path) are answered by calling the tool directly, in any mode, which saves a model round trip or two. The call goes through the same policy, approvals and audit log as a model's would, and both models are told about it the next time they answer. Anything phrased differently, or an app that isn't installed, goes to a model as before; `@claude` or `@local` in front always does. `/route <text>` shows which way a message goes, and `fast_path = false` under `[chat]` turns it off.

**When a model makes Orion wait**, the chat says so and counts it down: `waiting on Gemini's rate limit (17s, retry 1 of 3) · 15 requests in the last minute`. Where the API reports its allowance (Claude, OpenAI and other OpenAI-style servers do; Gemini doesn't), the notice says how many requests are left instead, and Orion warns once a minute when they are nearly used up.

## Approvals

When a call needs approval, Orion shows the tool and its full arguments:

```
  ? Approve update_apply?
    { "security_only": true }
  [y]es / [n]o / [a]lways for this session › y
```

- **`y`** runs this call; **`n`** declines it, and the AI is told; **`a`** approves this tool for the rest of the session.
- **Ctrl-C** at the prompt counts as no.
- **Hard checks come first**, so you're never asked about a call that would be refused anyway.
- **Read the arguments.** A `proc_run` shows the exact program and arguments; an `fs_write` the path and content; a `pkg_install` the package names. Unknown packages and impossible updates fail before the prompt.

To stop being asked about something routine, pre-approve it in the policy rather than answering `a` each session.

## Tuning the policy

The policy is `~/.config/orion/orion.toml`. Start from [`orion.example.toml`](../orion.example.toml), which lists every setting with its default. After an edit:

```sh
orion tools        # every tool and its decision, and the sandbox
```

The daemon picks up edits within two seconds; `orion chat` and `orion shell` read the policy when they start.

### Ready-made policies

Each of these is a complete file. Combine them as you like.

**Look, don't touch.** The AI can only read:

```toml
[defaults]
read = "allow"
write = "deny"
execute = "deny"
```

**A trusted helper.** Routine work runs without asking, everything else asks:

```toml
[proc]
allowed_commands = ["df", "free", "uname", "uptime", "lsblk", "nvidia-smi", "git", "journalctl"]

[services]
allowed = ["ollama.service", "llama-server.service"]   # svc_control may restart these without asking

[packages]
allowed = ["python3-*", "jq", "htop"]   # pkg_install without asking
updates = "security"                    # security updates without asking

[tools]
fs_write = "allow"                      # still only inside [fs] write_roots
```

**A shared workspace.** Let the AI read and write one project, and nothing else of yours:

```toml
[fs]
read_roots = ["~/projects/website"]
write_roots = ["~/projects/website"]
```

**An unattended agent over MCP.** When nobody will be there to [approve](#approvals-for-ai-agents), `ask` means no, so allow exactly what it should do:

```toml
[tools]
svc_control = "deny"        # look at services, never change them
fs_write = "allow"          # still only inside [fs] write_roots
proc_run = "allow"          # only the commands below
update_apply = "allow"      # only what [packages] updates covers

[proc]
allowed_commands = ["git", "make", "cargo"]

[packages]
updates = "security"        # security fixes only
```

On `allow`, nobody is asked, so each tool's pre-approval setting becomes a hard limit: `proc_run` runs only `allowed_commands`, `pkg_install` installs only `[packages] allowed`, and `update_apply` runs only what `[packages] updates` covers (`"security"`: security-only updates; `"all"`: any update that removes nothing; `"full"`: any update, also ones that remove or replace packages, never protected ones). Anything else is refused, and the agent is told why.

**Strict sandbox.** Programs get no network and no GPU (the default), and nothing runs without bubblewrap or Landlock:

```toml
[sandbox]
require = true
memory_mb = 4096
cpu_secs = 120
```

**A child's voice.** Someone enrolled as `kid` may ask questions and have the AI look things up, but not change anything or answer approvals:

```toml
[voice.people.kid]
tools = "read"
approve = false
```

### What no policy can change

- Files outside `[fs] write_roots` can't be written, and outside the readable roots can't be read. Paths are resolved first, so `..` and symlinks don't escape.
- Protected services (`[services] protected`: ssh, networking, the display manager, systemd itself, Orion) can't be controlled.
- Protected and essential packages can't be removed, by `pkg_remove` or by an update.
- The system's mounts (`/`, `/boot`, `/home`, …) can't be unmounted.
- Firewall changes that would cut off an SSH session into the machine are refused, and so is taking down the connection one runs over.
- Programs started by `proc_run` can't see your keys, Orion's state or its policy, and under bubblewrap can't reach the daemon's or the helper's socket.
- `root_run`, `sys_edit` and `proc_run` outside the sandbox ask you every time, whatever the policy or trust level says. `root_run` and `sys_edit` run only for the logins root lists in the helper's own `[helper] root_users`.

You can extend the protected lists in the policy. Shortening them is possible, so do it with care.

## Looking after the machine

You can ask for all of this in the chat; the calls below show the tools directly. Each `orion call` here is also exactly what the AI runs.

### Updates

```sh
orion call update_check                                   # pending updates, security fixes first, reboot needed?
orion call update_apply '{"security_only": true}'         # only security fixes
orion call update_apply '{"packages": ["openssl"]}'       # only these packages
orion call update_apply                                   # everything that removes nothing
orion call update_apply '{"full": true}'                  # also updates that remove or replace packages
```

Each result names its `change` in the package history and the snapshots taken; `pkg_rollback` with that id puts it back ([below](#undoing-package-changes)).

- **Nothing is removed unless you say `full`.** On apt such updates are held back and listed under `kept_back`. On dnf, zypper and pacman the update is refused and names what it would remove.
- **After an update**, the result lists services still running replaced code (`restart_services`), other processes (`restart_processes`) and whether to reboot, and why (`reboot_reasons`). Restart services with `svc_control`; Orion never reboots the machine.
- **Arch** upgrades everything or nothing, and has no security labels.
- **openSUSE Tumbleweed** is upgraded with `zypper dist-upgrade`.

A routine that works well: security fixes install unattended (below); you ask "check for updates" now and then and review the rest.

#### Unattended updates

The daemon installs updates once a day when the policy asks it to:

```toml
[packages]
updates = "security"        # security fixes need no approval: what makes unattended runs possible
automatic = "security"      # "all" for every update (with updates = "all")
automatic_at = "04:00"      # local time; a run missed while the machine was off happens at the next chance
automatic_on_battery = false
```

Each run refreshes the package lists, then calls `update_apply` like any other caller: through the policy (so `updates` must allow it, and nothing is ever removed), with snapshots and an entry in the package history. The audit log names the caller "unattended updates". `orion status` shows the setting and the last run:

```
updates:      unattended: security fixes, daily from 04:00, not on battery; last 2026-10-23 04:00: installed 3 updates (change 12)
```

A run that installed something sends a desktop notification naming the change, so `pkg_rollback` can undo it. A failed run (no network, a refusal by the policy) appears under [what needs attention](#what-needs-attention) with the reason, and the next day's run tries again. Orion never reboots; the reboot check says when one is due.

The daemon is a user service, so unattended updates run while you are logged in, or always with `loginctl enable-linger`. Root work goes through the [privileged helper](install.md#the-privileged-helper-recommended), or `sudo -n`. For a machine nobody logs into, the root timer below does the same without the daemon.

A timer of your own works as well, through the helper, with no root unit: set `[packages] updates = "security"`, then `timer_create '{"name": "security-updates", "schedule": "daily", "tool": "update_apply", "args": {"security_only": true}}'`.

#### Without the daemon: a root timer

With `[packages] updates = "security"` and `update_apply` left at `ask`, a security-only update needs no approval, and any other update still does. With nobody to ask, "any other" means refused. That makes a timer safe: it can install security fixes and nothing else.

As root, write a policy for it, `/etc/orion/updates.toml`:

```toml
[packages]
updates = "security"

[audit]
path = "/var/log/orion-updates.jsonl"
```

Then `/etc/systemd/system/orion-updates.service`:

```ini
[Unit]
Description=Orion: install security updates
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/local/bin/orion call --config /etc/orion/updates.toml update_check
ExecStart=/usr/local/bin/orion call --config /etc/orion/updates.toml update_apply '{"security_only": true}'
```

And `/etc/systemd/system/orion-updates.timer`:

```ini
[Unit]
Description=Orion: security updates every morning

[Timer]
OnCalendar=*-*-* 06:00
RandomizedDelaySec=30min
Persistent=true

[Install]
WantedBy=timers.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now orion-updates.timer
sudo systemctl start orion-updates.service     # once now, to check it
journalctl -u orion-updates                    # what was checked and installed, and what to restart
```

This needs the binary in `/usr/local/bin` ([installing for every user](install.md#1-install); with a release package, write `/usr/bin/orion` instead). It runs as root, so it needs no sudo rule. The first step refreshes the package lists; the second installs only security fixes, never removes a package, and says which services to restart and whether to reboot. Orion never restarts or reboots on its own.

`orion call` exits non-zero, so the service run shows as failed, when a call is refused, can't start, or the package manager fails partway. In that last case the result says `"ok": false` with the error, and the audit log records the failure. Arch has no security labels, so this does nothing there.

### Undoing package changes

Every install, removal and update Orion makes is written down, every package's version before and after, and snapshotted where snapper is set up:

```sh
orion call pkg_history                                    # newest first: what moved, the snapshots, what was put back
orion call pkg_rollback '{"id": 12}'                      # put change 12 back
orion call pkg_rollback '{"id": 12, "method": "packages"}'  # by the package manager, even with snapshots
```

- **With snapshots** (`snapper` with a configuration for `/`: openSUSE has one; on Fedora or Ubuntu with btrfs, `sudo snapper -c root create-config /`), the rollback undoes every file the change touched (`snapper undochange`). `pkg_history` says whether snapshots are on.
- **Without**, it installs the old versions again, from the repositories or pacman's cache, and removes what the change added. An old version that the repositories no longer carry can't come back this way; the refusal says which, before anything runs.
- **Changes go back newest first**: a rollback is refused while a later change to the same packages is in place, and the refusal names that change.
- **A reboot is wise** after putting back a kernel or core libraries; the result of the next `update_check` says so.

Root work (`root_run`, `sys_edit`) is in the same history and goes back the same way, by its snapshot pair only, and only while nothing later is in place. `[root] snapshots = "required"`, the default, refuses root work where no snapshot can be taken.

`[packages] snapshots = "required"` makes every package change wait for a snapshot, refusing it where none can be taken; `"never"` turns them off; `snapper_config` names another configuration than `root`.

### Files

```sh
orion call fs_find '{"path": "~", "name": "*.pdf"}'                 # by name, ignoring case
orion call fs_find '{"path": "~/notes", "contains": "dentist"}'     # by text inside, with the line
orion call fs_copy '{"from": "~/orion-workspace/report.md", "to": "~/orion-workspace/old"}'
orion call fs_move '{"from": "~/orion-workspace/draft.md", "to": "~/orion-workspace/final.md"}'
orion call fs_delete '{"path": "~/orion-workspace/old"}'           # to the Trash; "recursive" for a full folder
```

Changes stay inside `[fs] write_roots`. Symlinks are handled as links, never followed out of a root. A deleted file is in the Trash until you empty it; `"permanent": true` skips the Trash.

### Processes

```sh
orion call proc_list '{"sort": "memory", "limit": 10}'
orion call proc_signal '{"name": "firefox"}'                        # term, then wait up to 5 s
orion call proc_signal '{"pid": 4242, "escalate": true}'            # and kill it if term isn't enough
orion call proc_signal '{"name": "ffmpeg", "signal": "stop"}'       # pause; "cont" resumes
```

PID 1, kernel threads, Orion itself and what it runs under, protected services' processes and `[proc] protected` are never signalled. A process a service runs may come back; stop the service with `svc_control` instead.

### The journal

```sh
orion call journal_read '{"priority": "err", "since": "today"}'     # errors and worse, today
orion call journal_read '{"unit": "ollama", "since": "1h ago"}'
orion call journal_read '{"boot": -1, "priority": "warning", "lines": 200}'   # why did the last boot go wrong?
orion call journal_read '{"kernel": true, "grep": "usb"}'
```

Seeing other users' and the system's entries needs the `systemd-journal` (or `adm`) group: `sudo usermod -aG systemd-journal "$USER"`.

### Timers

```sh
orion call timer_list
orion call timer_create '{"name": "disk-report", "schedule": "Mon 09:00", "tool": "proc_run", "args": {"command": "df", "args": ["-h"]}}'
orion call timer_remove '{"name": "disk-report"}'
```

A timer runs its tool call through the policy and audit log when it fires, with nobody there to approve it, so the call must be pre-approved. For the one above, `df` goes in `[proc] allowed_commands`; `timer_create` refuses until it is. Each run is in the audit log as `"caller": "timer disk-report"`, and in `journalctl --user -u orion-timer-disk-report`.

### Services

```sh
orion call svc_list '{"state": "failed"}'
orion call svc_status '{"unit": "ollama"}'               # state, last result, memory, the last log lines
orion call svc_control '{"unit": "ollama", "action": "restart"}'
```

Add `"scope": "user"` for your own user services.

### Packages

```sh
orion call pkg_search '{"query": "ripgrep"}'
orion call pkg_info '{"name": "ripgrep"}'
orion call pkg_install '{"packages": ["ripgrep"]}'
orion call pkg_remove '{"packages": ["ripgrep"]}'
```

### Disks

```sh
orion call disk_list                                      # disks, partitions, mounts, how full
orion call disk_usage '{"path": "~"}'                     # what is filling a folder
orion call disk_mount '{"device": "LABEL=BACKUP"}'
orion call disk_unmount '{"target": "/media/you/BACKUP", "eject": true}'
```

### Network and firewall

```sh
orion call net_info
orion call wifi_scan
orion call fw_status
orion call fw_rule '{"action": "allow", "port": "8080", "proto": "tcp", "from": "192.168.1.0/24"}'
```

### GPUs and local models

```sh
orion call gpu_info                                       # every GPU, what runs on each, down to the model
orion call model_status                                   # inference servers, loaded models, the VRAM budget
orion call model_load '{"model": "qwen3:32b", "evict": true}'
orion call gpu_assign '{"unit": "ollama", "gpus": [1]}'   # pin Ollama to GPU 1
```

The argument names above are the common ones; `orion tools` and each tool's schema (the AI sees it, and errors name a wrong argument) have the rest.

### Sound, screens, power and apps

```sh
orion call audio_set '{"change": 10}'                       # louder (pre-approved)
orion call audio_set '{"device": "HDMI", "make_default": true}'
orion call display_set '{"brightness": 60}'                 # the panel (pre-approved)
orion call display_set '{"output": "DP-1", "mode": "2560x1440@144", "scale": 1.25}'
orion call power_status                                     # charge, time left, battery health, profile
orion call power_action '{"action": "lock"}'                # pre-approved; suspend and reboot ask
orion call bt_status '{"scan_secs": 10}'                    # put the headphones in pairing mode first
orion call bt_device '{"action": "pair", "address": "38:18:4C:12:34:56"}'
orion call app_open '{"app": "firefox", "targets": ["https://example.org"]}'
orion call clock_set '{"timezone": "Europe/Berlin"}'
```

To let the AI do more of these without asking, add kinds to `[desktop] approved` (`displays`, `power_profile`, `suspend`, `bluetooth`, `clock`, `open`, …) and apps to `[desktop] apps`. Under `orion serve`, where nobody is asked, those are the only desktop changes agents can make.

When a desktop tool says no session is running, the daemon can't see your desktop: log in graphically, or check that your desktop imports its environment into systemd (`systemctl --user show-environment` should list `WAYLAND_DISPLAY` or `DISPLAY`). Most do; for a bare compositor, add `exec systemctl --user import-environment WAYLAND_DISPLAY` to its config.

## What needs attention

The daemon checks the machine every 15 minutes and keeps what it finds: failed services, filesystems 90% full, pending security updates, a reboot that is due, memory under 5%, parts running hot, a battery holding under 70% of its charge, a program logging 50 errors in 15 minutes, any hardware error the kernel reports (CPU, memory, filesystem, disk I/O, GPU, throttling, out of memory), and 10 failed logins in an hour. Every 6 hours it also reads each drive's SMART data.

```sh
orion status                                      # the findings, each with what to do about it
orion call system_health                          # checks afresh, including updates
orion call voice_check                            # how listening is going, and what to change
orion call settings_check                         # rate limits, slow speech to text, approvals always given
```

```
attention:    2 things (checked 09:14:11)
  !  /home is 93% full: 12 GiB free
     disk_usage shows what fills it; fs_delete moves things to the Trash
  !  3 security updates pending
     update_apply with security_only installs them
```

- A **desktop notification** comes once per problem, when it first appears.
- **The AI is told** at the start of each conversation, and brings it up once when it fits. Ask "how is the machine doing?" for a fresh check.
- **The chat and the shell** show the findings: under the header, in the full-screen panel (Ctrl-B), in the shell's machine panel.
- **Too noisy?** Raise `[watch] disk_percent`, or leave findings out by key with `ignore`, such as `["disk:/mnt/archive", "service:backup.service"]`. `notify = false` keeps the checks but stops the notifications; `enabled = false` stops the watch.
- **Without the daemon,** `orion status` and the interfaces run the quick checks themselves, all but the update list and disk health.
- **Disk health needs** smartmontools, and root through the [privileged helper](install.md#the-privileged-helper-recommended), which runs only `smartctl --json=c -a --nocheck=standby` on a whole disk. Sleeping disks are left asleep. Virtual disks have no SMART data and show as unknown, which is never a finding.
- **The error and login checks** read the system journal, which needs membership of `systemd-journal` (`adm` or `wheel` on some distributions); without it they see only your own programs.
- **A finding about errors in the journal** lasts as long as they keep coming; once a 15-minute window is quiet, it clears, and it notifies again if they return.

## The audit log

Every tool call, by any model, agent or person, is one JSON line in `~/.local/state/orion/audit.jsonl`:

```json
{"approved":true,"args":{"security_only":true},"caller":null,"decision":"ask","duration_ms":2457,"error":null,"ok":true,"tool":"update_apply","ts_ms":1792465136288}
```

| Field | |
|---|---|
| `tool`, `args` | what was called, with secrets blanked (Wi-Fi passwords show as `••••••`) |
| `decision` | `allow`, `ask` or `deny` |
| `approved` | your answer to an `ask`, or `null` when nobody was asked |
| `caller` | who asked: an MCP client and its pid (`"claude-code (pid 5312)"`), or a person by voice (`"ana (by voice)"`) |
| `ok`, `error`, `duration_ms` | how it went: `ok` is false when the call was refused or failed, or when what the tool drove failed (a package manager partway), and `error` says why |
| `ts_ms` | when, in milliseconds since 1970 |
| `prev` | the SHA-256 of the entry before, which chains the log |

```sh
orion audit -n 50                                          # the last 50 calls
orion audit -n 1000 | jq -c 'select(.ok == false)'          # failures
orion audit -n 1000 | jq -c 'select(.decision == "ask") | {tool, approved, args}'
orion audit -n 1000 | jq -r '.tool' | sort | uniq -c | sort -rn   # which tools get used
```

### Checking nothing was changed

Each entry carries the hash of the one before it, so changing, removing or inserting an entry breaks the chain at the next one. The privileged helper also keeps checkpoints, where code running as you can't reach them: as the log grows, the daemon sends it the number of entries and the last one's hash (at most once a minute), and it appends them to `/var/lib/orion/audit-anchors/UID.jsonl`, root's file. A checkpoint catches what the chain alone can't: the last entries removed, or the whole log rewritten.

```sh
orion audit --verify
```

```
audit log:    /home/you/.local/state/orion/audit.jsonl (1532 entries)
chain:        intact
checkpoints:  41 by the helper, all hold; the latest 2026-10-24 09:15:02 (entry 1530)
```

When something was changed, the lines marked `!!` say which entry and how, and the command exits 1, so a timer or monitoring script notices. `orion audit --checkpoint` takes a checkpoint now, without the daemon. Without the helper there are no checkpoints, and the chain alone shows edits but not truncation. Entries written before Orion chained the log are accepted as they are.

The log only grows. Rotate it with logrotate, or move it aside when Orion isn't running; a rotated log starts a new chain, and the old checkpoints then no longer hold, so verify the old log before rotating. `[audit] enabled = false` turns it off, which isn't recommended.

## Conversations and memory

Conversations are saved after every message:

```sh
orion chat --resume                # the most recent
orion conversations                # all of them: id, last active, length, title
orion conversations show <id>
orion conversations delete <id>
```

Each person has their own conversations and memory. The AI saves lasting facts with its `memory_save` tool ("Prefers metric units") and is reminded of them in each new conversation.

```sh
orion memory                       # what is remembered about you
orion memory --person ana
orion memory forget 3
orion memory clear
```

Things that look like secrets are never saved. Notes the router considers private are shown only to the local model. `[chat] save_conversations = false` and `memory = false` turn these off.

## Voice

- **Push to talk:** Ctrl-T in the chat, or the mic button in the shell. Your words land in the message box to check before sending.
- **Spoken replies:** `/voice speak on`.
- **Hands-free:** Alt-W, or `wake = true` under `[voice]`, then "Orion, what's using the GPU?". A bare "Orion" chimes and waits for the request. "Orion, stop" interrupts.
- **Only enrolled voices** wake Orion once someone is enrolled (`orion voice enroll`). `orion voice status` lists who, and `orion voice test` shows who Orion takes you for.
- **Spoken approval** ("Orion, yes") is off unless `wake_approvals = true`, since anything playing audio nearby could say it. "No" always declines.
- **Headless:** `orion listen`, or the `orion-voice` user service. `journalctl --user -u orion-voice -f` shows what it heard and said.

Run only one listener at a time: wake listening in a chat or shell, or `orion listen`, not both.

## The daemon

With the daemon running, every agent shares one policy, audit log and sandbox, and policy edits apply live.

```sh
systemctl --user status orion.socket orion.service
orion status                        # running? socket, policy, tool counts, sandbox, agents, calls
journalctl --user -u orion -f       # its log
systemctl --user restart orion      # after upgrading Orion
```

`orion status` exits 1 when the daemon isn't running, so scripts can check it. A policy edit with a mistake is reported in `orion status`, and the previous policy stays in force until it's fixed.

`orion serve` connects to the daemon when it runs. `orion serve --direct` serves in-process anyway.

The chat, the shell and `orion listen` call tools through the daemon too, when it runs this version of Orion with the same policy file: their header or machine panel says `tools run through the daemon (pid …)`. If it says why not ("the daemon uses another policy", "the daemon is version …: restart it"), they call tools in-process as before. `[daemon] chat = "never"` always keeps them in-process.

### Approvals for AI agents

Through the daemon, AI agents (Claude Code, or any MCP client launching `orion serve`) are offered the `ask` tools too. Each call waits for a person:

```sh
orion approvals              # answer in this terminal
orion approvals --notify     # answer from desktop notifications
orion approvals --list       # what waits now
```

To always answer by notification:

```sh
cp contrib/orion-approvals.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now orion-approvals.service
```

The graphical shell shows the same requests as cards, naming the agent. Answer wherever is handy; the first answer counts. With nobody watching, the call is refused at once and the agent is told why. With nobody answering for `[daemon] approval_timeout_secs` (120), it counts as no. `orion status` shows how many are watching and how many requests wait.

`[daemon] approvals = false` goes back to offering agents only the `allow` tools.

## Files and settings

| Path | What |
|---|---|
| `~/.config/orion/orion.toml` | the policy (`ORION_CONFIG` or `--config` override it) |
| `~/.config/orion/env` | keys such as `ANTHROPIC_API_KEY`, one `VAR=value` per line: the systemd services and every `orion` command read it (a variable your shell exports wins) |
| `~/.local/state/orion/audit.jsonl` | the audit log |
| `~/.local/state/orion/conversations/` | saved conversations |
| `~/.local/state/orion/memory/` | what the AI remembers, one file per person |
| `~/.local/state/orion/voices/` | voiceprints, one file per person |
| `~/.local/state/orion/chat_history` | your typed message history |
| `~/.local/state/orion/shell-url` | the running shell's address and token |
| `~/.local/state/orion/pacman-db/` | Arch: the private copy of the package lists |
| `~/.local/state/orion/packages.jsonl` | the package history: every change Orion made, for `pkg_history` and `pkg_rollback` |
| `~/.local/state/orion/unattended.json` | the last unattended update run |
| `~/.local/share/orion/models/` | the neural speaker model |
| `~/orion-workspace/` | where the AI writes, by default |
| `$XDG_RUNTIME_DIR/orion/orion.sock` | the daemon's socket |
| `/run/orion/helper.sock` | the privileged helper's socket (root and the `orion` group) |
| `/etc/orion/helper.toml` | the privileged helper's own policy, root's only |

`XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `XDG_DATA_HOME` move these as usual. The state directory is readable only by you and hidden from sandboxed programs.

| Variable | |
|---|---|
| `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` | Claude credentials |
| `ANTHROPIC_BASE_URL` | a different Claude endpoint |
| `ORION_CONFIG` | the policy file |
| `ORION_LOG` | log detail for Orion's own messages, e.g. `ORION_LOG=debug` (default `info`) |
| `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY` | a proxy for Claude and downloads; servers on `localhost` or `127.*`, and hosts in `NO_PROXY`, are reached directly |

## Troubleshooting

Start with `orion doctor`. It checks, in order: which policy file Orion reads (and a misspelled one nearby), whether it loads (curly quotes, unknown keys), the env file (its permissions, `export` lines, stray characters in keys, an older key your shell exports that wins over it), each configured model with a live call (a turned-down key, a retired or misspelled model name), the microphone, the transcriber, the wake-word detector, the sandbox, the privileged helper's group and socket (including a group you were added to but haven't logged in again for), and whether the daemon has the current keys. For each problem it says what to do, and offers to do it: it asks before each fix (`--yes` takes them all, `--no-fix` only reports), keeps a `.bak` of any file it changes, and exits 1 while something is still broken.

Then these three answer most other questions:

```sh
orion tools            # what the policy allows, and the sandbox in use
orion status           # the daemon, and how Orion gets root
orion audit -n 20      # what was called, and how it went
```

**`no Claude credentials: set ANTHROPIC_API_KEY …`.** Export the key in this shell, or put it in `~/.config/orion/env` for the services. Or use `orion chat --local`.

**`cannot connect to http://127.0.0.1:11434/v1: is the server running?`.** The local model server isn't up: `systemctl status ollama`, or start yours. If it runs elsewhere, set `[chat.local] base_url`.

**`… has no models; install one first`.** `ollama pull qwen3:8b`.

**The local model doesn't use tools, or forgets what it did.** Use a tool-calling model (Qwen3, Llama 3.1+, Mistral Small 3, gpt-oss) of at least about 7B, and raise the context length (`OLLAMA_CONTEXT_LENGTH=32768`).

**`tool X needs interactive approval, which is not available here`.** The tool is `ask` and nobody can be asked (`orion call` without a terminal, `orion serve --direct`, a script). Run it from `orion chat`, or set it to `allow` under `[tools]`.

**An agent says "nobody is approving right now".** Its call went through the daemon, and nobody was watching for approval requests. Run `orion approvals`, open the shell, or enable `orion-approvals.service`, and let the agent try again.

**`"X" is not in packages.allowed`, or `packages.updates = "security" lets only security_only updates run without approval`.** With a tool on `allow`, its pre-approval setting is a hard limit. Widen the setting (`[packages] allowed`, `[packages] updates`), or set the tool back to `ask` to be asked instead.

**`an update with full may remove packages, so a person must approve it …`.** `update_apply` is on `allow`, where nobody is asked. Run that update from the chat with the tool on `ask`, or set `[packages] updates = "full"` if unattended updates may remove packages.

**`orion call` printed a result, then `… failed: …`, and exited 1.** The tool ran, but what it drove failed partway, typically the package manager. The `error` in the result has its last lines; fix the cause (a broken package, a full disk, another package manager holding the lock) and run it again.

**`the Orion helper isn't running at /run/orion/helper.sock …`.** `sudo systemctl enable --now orion-helper.socket`, or see [the install guide](install.md#the-privileged-helper-recommended).

**`you may not use the Orion helper at …: join its group`.** `sudo usermod -aG orion "$USER"`, then log out and back in: group changes apply to new logins. `id` shows your groups.

**`the Orion helper refused: …`.** The helper checked the command on its side and said no; the reason follows, and `journalctl -u orion-helper` has the details. A protected package, unit or mount is refused by the helper's own policy, `/etc/orion/helper.toml`, whatever yours allows.

**`sudo: a password is required (sudo needs a password here; see [admin] elevate …)`.** Orion uses `sudo -n` and never prompts for a password. Add the sudoers rule from [the install guide](install.md#6-root-access-for-the-machine-tools), set `[admin] elevate = "pkexec"` on a desktop, or do that task yourself.

**`sudo: The "no new privileges" flag is set …` in the daemon's tools.** The hardened `orion.service` blocks sudo. Remove `NoNewPrivileges=yes` from `~/.config/systemd/user/orion.service`, then `systemctl --user daemon-reload` and `systemctl --user restart orion`.

**`… is outside the writable roots`.** The AI tried to write somewhere it may not. Widen `[fs] write_roots` if it should be allowed; approving can't override this.

**`this update would remove X (replaced or obsoleted); set full to allow that`.** The update replaces or removes a package. Check what X is, then run it with `full: true` if that's fine (`update_apply '{"full": true}'`), or update only the other packages by name.

**`… is protected` / `… is essential`.** By design: no approval removes or stops it. Edit the protected list in the policy if you really mean it.

**`orion tools` says Landlock or limits only, though bubblewrap is installed.** bubblewrap couldn't create a sandbox here, which usually means unprivileged user namespaces are restricted. Check `sysctl kernel.unprivileged_userns_clone` (Debian) and `sysctl kernel.apparmor_restrict_unprivileged_userns` (Ubuntu 23.10+); inside containers they're often unavailable. Landlock still confines files and TCP. `[sandbox] mode = "bubblewrap"` turns the fallback into an error (`sandbox.mode is bubblewrap, but bwrap is missing or can't create a sandbox here`).

**The policy doesn't load.** The error points at the line, and for a misspelled key lists the valid ones:

```
TOML parse error at line 2, column 1
  |
2 | update = "all"
  | ^^^^^^
unknown field `update`, expected one of `manager`, `allowed`, `protected`, `updates`, `timeout_secs`
```

**The shell's address stopped working.** Each run of `orion shell` makes a new token. Use the address it printed, or the one in `~/.local/state/orion/shell-url`. The shell listens on `127.0.0.1` only; from another machine, use an SSH tunnel: `ssh -L 7310:127.0.0.1:7310 you@host`.

**`an Orion daemon is already running on …`.** One is enough: `orion status`. With the socket unit enabled, systemd starts it for you.

**Voice: `set voice.transcribe_url or voice.transcribe_command`, or nothing is heard.** Run `/voice` in the chat: it lists the microphone, transcriber and speech it found. `the recording is empty: is a microphone connected?` means the recorder got no audio: check the input device in your sound settings, or set `[voice] record_command`.

**Voice: Orion keeps listening after you stop, or misses quiet speech.** Background noise is crossing the speech threshold, or your voice isn't. Ask Orion "check my voice settings": `voice_check` looks at how your recent requests ended and suggests a change, which `voice_tune` makes once you approve it. Or `orion voice noise` measures both and prints `noise_margin` and `speech_level` values to put under `[voice]`. If it says the room is as loud as your voice, move closer to the microphone, or use a wake-word detector (`orion voice setup --from wake` installs one).

**Replies are slow, or `waiting on …'s rate limit`.** Ask Orion "is anything slowing you down?": `settings_check` counts the rate limits of the last day and how long speech takes to transcribe, and suggests a lighter model, less thinking or a smaller Whisper model, which `settings_tune` sets once you approve it. Undo it the same way, or delete `~/.config/orion/tuning.toml`.

**The wake detector stopped.** Run the `wake_command` by hand with `--verbose` (for `orion-wake`) to see its error: usually a missing `pip install --user vosk`, or no microphone.

**Wake listening won't start with a remote transcriber.** It would send everything said near the microphone off the machine. Run the transcriber locally, or set `wake_allow_remote = true` if that's what you want.

**`… is from another version of Orion; enroll again`.** Run `orion voice enroll` (with `--name` for others) again.

**Arch: `update_check` shows `refresh_error`.** The private refresh needs root, or `fakeroot` installed when Orion runs as a normal user.

**Still stuck?** `ORION_LOG=debug orion chat 2> ~/orion-debug.log` writes more detail to a file (Orion logs to standard error).

## Security checklist

- **Keep `write_roots` small.** The default, `~/orion-workspace`, is a good start.
- **Pre-approve narrowly.** `allowed_commands`, `services.allowed` and `packages.allowed` are better than turning whole tools to `allow`.
- **Install bubblewrap** and consider `[sandbox] require = true`.
- **Grant root through the helper.** It does only Orion's operations, checked against a policy only root can change. A passwordless sudo rule lets anything running as you become root, not only Orion.
- **Add yourself to `[helper] root_users` only if you want root work** (`root_run`, `sys_edit`). It makes the helper as good as sudo for code running as you; Orion still asks before each step, and each is snapshotted.
- **Leave `wake_approvals` off** unless only trusted people can be heard, and enroll voices so others can't wake Orion.
- **Keep private things private:** add your own sensitive folders to `chat.router.private_paths` (Claude never sees them) and `[sandbox] hide` (programs never see them).
- **Read the audit log** now and then, especially after letting an agent run unattended, and run `orion audit --verify`: with the helper, nothing running as you can rewrite it unnoticed.
- **Keep the seccomp filter and scopes on** (`[sandbox] seccomp`, `cgroup`): `orion status` should say "system calls filtered, a cgroup per program".
- **Protect `~/.config/orion/env`** (`chmod 600`): it holds your API key.
