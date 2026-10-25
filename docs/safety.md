# Safety and control

How Orion decides what the AI may do, and how to undo what it did.

- [How a tool call is checked](#how-a-tool-call-is-checked)
- [Trust levels](#trust-levels)
- [What always asks](#what-always-asks)
- [Hard limits](#hard-limits)
- [The policy file](#the-policy-file)
- [The sandbox](#the-sandbox)
- [Root work](#root-work)
- [Developer mode](#developer-mode)
- [Snapshots and rollback](#snapshots-and-rollback)
- [Private things](#private-things)
- [The audit log](#the-audit-log)

## How a tool call is checked

The AI never touches the machine directly. It asks Orion to call a tool, and every call goes through four steps:

1. **The policy decides:** `allow` (run it), `ask` (ask you first) or `deny` (the tool isn't even offered).
2. **Hard checks run.** Some things no approval can override, such as writing outside Orion's folders. Those calls fail before you are asked.
3. **The tool runs.** Programs the AI asks for run in the [sandbox](#the-sandbox).
4. **The call is logged,** with its arguments (secrets blanked), the decision, your answer and the result.

## Trust levels

A trust level is a ready-made answer to "how much may Orion do without asking?". `orion setup` asks for one. To change it later:

```sh
orion setup --trust ask          # Ask me everything
orion setup --trust everyday     # Everyday
orion setup --trust hands-off    # Hands-off
```

| Level | Runs without asking | Still asks |
|---|---|---|
| **Ask me everything** | nothing that changes the machine, not even the volume | every change |
| **Everyday** | Flatpak installs, volume and brightness, locking the screen, opening apps and files, and starting or restarting the services you listed in `[services] allowed` (or Ollama and llama-server, if you listed none) | package installs, updates, files, programs, everything else |
| **Hands-off** | everything: files, programs, packages, services, updates, rebooting | only [what always asks](#what-always-asks) |

What each level writes into `orion.toml`:

| Level | Settings |
|---|---|
| Ask me everything | `[defaults] write` and `execute` = `ask`; `[desktop] approved`, `apps`, `[flatpak] allowed`, `[packages] allowed`, `[services] allowed` and `[proc] allowed_commands` emptied; `[packages] updates = "ask"` |
| Everyday | `[desktop] approved = ["volume", "brightness", "lock", "open"]`, `apps = ["*"]`, `[flatpak] allowed = ["*"]`, and `[services] allowed` as above. Package installs, updates and your `allowed_commands` are left as they were |
| Hands-off | `[defaults] write` and `execute` = `allow`, every allow-list `["*"]`, `[packages] updates = "all"` |

Good to know:

- **The old file is kept** as `orion.toml.bak`.
- **`orion doctor` says which level you're on.** If you changed things by hand, it names the nearest level and what's different.
- **A `[tools]` entry set to `allow` stays as it is.** Setup and doctor point out any that let a tool run unasked beyond your level.
- **Under Hands-off,** an update that would remove packages is refused rather than run.
- **The [hard limits](#hard-limits) are the same at every level.**

## What always asks

These ask you every time, at every trust level, whatever the policy says:

- `root_run`: run a program as root,
- `sys_edit`: write a system file as root,
- `proc_run` with `"sandbox": false`: run a program as you, outside the sandbox,
- `settings_tune`: change a setting Orion [tuned](setup.md#self-tuning),
- `dev_shell` opening a shell in [unsandboxed developer mode](#developer-mode).

For the first three, "always for this session" approves only that one call, they can't be put on a timer, `orion serve` won't run them with nobody there, and a spoken "yes" doesn't count. They are meant to be read on a screen.

## Hard limits

Orion refuses these before you are asked. No approval or trust level changes them:

- **Files** outside `[fs] write_roots` (default `~/orion-workspace`) can't be written, and files outside the readable roots can't be read. Paths are resolved first, so `..` and symlinks can't escape.
- **Protected services** (SSH, networking, the display manager, systemd itself, Orion) can't be stopped or changed.
- **Protected and essential packages** (the kernel, systemd, sudo, glibc, the package manager, bubblewrap, …) can't be removed, directly or by an update.
- **The system's own disks** (`/`, `/boot`, `/home`, …) can't be unmounted.
- **SSH can't be cut off:** no firewall rule or network change that would drop an SSH session into the machine.
- **Processes the system needs** (PID 1, systemd, sshd, the display server, Orion) can't be stopped.
- **Flatpak apps** come only from `[flatpak] remotes` (Flathub by default).

You can add to the protected lists in the policy. You can also shorten them, so do that with care.

## The policy file

The policy is `~/.config/orion/orion.toml`. `orion setup` writes it, and you can edit it. [`orion.example.toml`](../orion.example.toml) lists every setting with its default.

After an edit, run `orion tools` to see what each tool will do. The daemon picks up edits within two seconds; restart `orion chat` or `orion shell` to see them there. A misspelled setting is an error, so a typo can't silently do nothing, and `orion doctor` points at the line.

The main sections:

| Section | What it sets |
|---|---|
| `[defaults]` | `allow`, `ask` or `deny` for each risk: `read`, `write`, `execute` |
| `[tools]` | the same, for one tool: `fs_write = "allow"` |
| `[fs]` | `read_roots` and `write_roots` |
| `[proc]` | programs that run without asking (`allowed_commands`), timeouts, `unsandboxed` |
| `[packages]` | packages that install without asking, `updates`, unattended updates, snapshots |
| `[flatpak]` | apps that install without asking, and where they may come from |
| `[services]` | units that may be controlled without asking, and protected units |
| `[desktop]` | desktop changes that need no approval (`volume`, `brightness`, `lock`, …), and apps |
| `[sandbox]` | the sandbox |
| `[root]` | root work: snapshots and time limit |
| `[chat]`, `[voice]`, `[watch]` | the chat, voice, and what Orion watches for |

[Operating Orion](operating.md#ready-made-policies) has ready-made policies.

## The sandbox

Every program the AI starts with `proc_run` runs in a sandbox, whatever the policy allows it to run.

With **bubblewrap** (best; on Fedora `sudo dnf install bubblewrap`):

- the whole system is read-only, except Orion's writable folders and a private `/tmp`;
- your keys and secrets (`~/.ssh`, `~/.gnupg`, cloud credentials, Orion's own files) are hidden;
- no network and no GPU;
- Orion's own sockets are out of reach, so a program can't approve its own requests;
- limits on memory (8 GB), CPU time (5 minutes), file size and processes, and dangerous system calls refused.

Without bubblewrap, Orion uses the kernel's Landlock, which is weaker (it can't hide sockets, and blocks only TCP). `orion tools | tail -1` shows which one is in use.

| | bubblewrap | Landlock | limits only |
|---|---|---|---|
| Files | read-only, except the writable roots and a private `/tmp` | read-only, except the writable roots, `/tmp`, `/var/tmp`, `/dev/shm` | as you |
| Private paths (`~/.ssh`, `~/.gnupg`, …) | hidden | unreadable | readable |
| Network | none | no TCP connect or bind | open |
| GPU | no devices | no devices | open |
| Orion's sockets | hidden | reachable | reachable |
| Memory, CPU time, file size limits; no privilege gain | ✓ | ✓ | ✓ |
| Dangerous system calls refused (seccomp) | ✓ | ✓ | ✓ |
| A cgroup per program (memory, at most 512 processes) | ✓ | ✓ | ✓ |

Settings under `[sandbox]`: `network = true` or `gpu = true` open those up; `memory_mb`, `cpu_secs` and `max_file_mb` change the limits; `require = true` refuses to run programs with no sandbox at all.

The machine tools (packages, services and so on) run their own fixed commands outside the sandbox. They take checked arguments, not programs, and have their own protected lists.

## Root work

For jobs no other tool covers, such as a setting in `/etc`, `sysctl`, adding a repository, or `restorecon`, Orion can work as root. You approve each step, and each step can be undone.

### The three tools

- **`sys_edit`** writes a system file as root. You approve it as a diff against what the file holds now.
  - An existing file keeps its owner, mode and SELinux label. A new file is root's, `0644` by default.
  - Not allowed under `/home`, `/root`, `/boot`, `/efi`, `/tmp`, `/proc`, `/sys`, `/dev`, `/run`, `/mnt` or `/media` (snapshots don't cover those), nor through a link, nor in a folder someone other than root can write. Up to 64 KiB.
- **`root_run`** runs one program as root, with no shell.
  - Only a program from `/usr/sbin`, `/usr/bin`, `/sbin` or `/bin`, or one only root could have put in place.
  - Clean environment, home folders read-only, stopped after 10 minutes (`[root] timeout_secs`).
- **`proc_run` with `"sandbox": false`** runs a program as you, outside the sandbox: with the network, the GPU and your files, for a build that downloads, say. No root. `[proc] unsandboxed = false` turns it off.

All three [always ask](#what-always-asks).

### Turn root work on

`root_run` and `sys_edit` are off until root allows your login. You need the privileged helper first (`bootstrap.sh` sets it up; see [install](install.md#the-privileged-helper-recommended)).

**1. Open the helper's policy as root:**

```sh
sudo mkdir -p /etc/orion
sudoedit /etc/orion/helper.toml
```

**2. Add your login under `[helper]`.** If the file already has a `[helper]` section, add the line to it rather than adding a second `[helper]`:

```toml
[helper]
root_users = ["your-login-name"]
```

(`whoami` prints your login name.)

**3. Restart the helper:**

```sh
sudo systemctl restart orion-helper.service
```

**4. Set up snapshots, once.** On Fedora with btrfs (the default):

```sh
sudo dnf install snapper
sudo snapper -c root create-config /
```

**5. Make sure bubblewrap is installed:**

```sh
rpm -q bubblewrap || sudo dnf install bubblewrap
```

**6. Check:** `orion doctor`.

### Why it's set up this way

- **Only root can turn it on.** `/etc/orion/helper.toml` belongs to root, so code running as you can't add itself. With no `root_users`, the helper does only Orion's own fixed operations.
- **Every step is snapshotted.** A snapper snapshot is taken before and after each one, described "orion root_run" or "orion sys_edit". `[root] snapshots = "required"` (the default) refuses root work where no snapshot can be taken.
- **It needs bubblewrap.** Bubblewrap hides the helper's socket from sandboxed programs. Landlock can't.
- **The helper logs each one** with who asked: `journalctl -u orion-helper`.

**Know what it grants.** With your login in `root_users`, any program running as you can ask the helper for root, so this is like a passwordless sudo rule, with a log and snapshots. Orion still asks you before each step. That's also why `proc_run` outside the sandbox, and apps `app_open` starts, deserve the same care as `root_run`: they run as you, and can reach the helper too.

## Developer mode

Safe Assistant mode, everything above, is the default and what Orion goes back to. Developer mode is for building and testing software: the AI gets a **persistent shell** in a folder of its own, `~/orion-dev`, with the network on, so it can fetch dependencies, build, run the tests, fix and go again without asking you at every step.

### Turn it on and off

Only you can switch, from a terminal. No tool switches modes, so the AI can't.

```sh
orion mode dev                      # sandboxed, for 8 hours
orion mode dev --for 2h             # or 30m, 2d, never
orion mode dev --unsandboxed        # the shell runs as you, for 1 hour
orion mode safe                     # back to Safe Assistant mode; open shells close
orion mode                          # which mode is on, and for how long
```

It turns itself off when the time is up (`[dev] expires`, `unsandboxed_expires`). `orion status`, `orion doctor`, `orion tools` and the chat's status show which mode is on.

### The two kinds of shell

| | Safe Assistant mode | `orion mode dev` | `orion mode dev --unsandboxed` |
|---|---|---|---|
| Programs | one at a time (`proc_run`) | a persistent bash (`dev_shell`): `cd`, variables, virtual environments and background jobs carry over | the same |
| Runs as | you, in the sandbox | you, in the sandbox | you, in namespaces of its own |
| Writes | `~/orion-workspace` | `~/orion-dev` and the toolchain caches in `[dev] caches` (`~/.cargo`, `~/.npm`, pip, Go, Maven, Gradle) | everything you can write, except below |
| Network | off | on | on |
| GPU | off | off (`[dev] gpu = true` turns it on) | on |
| Your keys (`~/.ssh`, `~/.gnupg`, cloud credentials) | hidden | hidden | readable, not writable: `git push` works |
| Orion's own files (settings, audit log, checkpoints, conversations) | hidden | hidden | hidden |
| Orion's and the root helper's sockets | unreachable | unreachable | unreachable |
| The user bus and systemd user manager | reachable | unreachable | unreachable |
| Login scripts (`~/.bashrc`, `~/.profile`, `~/.config/systemd`, autostart, …) | read-only | read-only | read-only (`[dev] protect`) |
| The machine's other processes | invisible | invisible | invisible |
| Debuggers (`gdb`, `strace`) | refused | work, inside the shell | work, inside the shell |
| Time per command | 30 s | as long as it takes: a call waits up to 30 minutes, then `dev_shell_read` follows it | the same |
| Memory | 8 GB per program | 16 GB for the whole shell (`[dev] memory_mb`) | the same |

Both need bubblewrap: Landlock can't hide sockets.

### What it asks

- **Opening a shell asks you.** The prompt says which kind. A sandboxed one may be approved "always for this session". An unsandboxed one asks every time it opens, like `root_run`.
- **Commands in an open shell don't ask**, at any trust level. The shell's boundary is the sandbox, not a prompt per command.
- **Everything else is unchanged.** Your trust level keeps deciding the machine tools, `root_run`, `sys_edit` and `proc_run` outside the sandbox still ask every time, `sudo` doesn't work in either shell, and the [hard limits](#hard-limits) hold. `[tools] dev_shell = "deny"` turns the shell off altogether.

### The audit log

Every command is its own entry (tool `dev_shell`), with the shell's id, the command line and how it ended, so `orion audit` reads like a shell history. Turning the mode on or off is logged too (tool `dev_mode`). The entries are in the same hash chain as everything else.

### Undo

Before each command Orion checkpoints `~/orion-dev`: every file's content, kept once, in Orion's state folder where the shells can't reach. Just ask ("undo what the last command did"), or:

```sh
orion call dev_history                  # newest first: when, and before which command
orion call dev_rollback '{"id": 12}'    # put ~/orion-dev back as it was then
```

- A rollback takes a checkpoint first, so it can be undone too.
- Folders named in `[dev] skip` (`target`, `node_modules`, `.venv`, …) and files over 64 MB are left out and left alone: a build makes them again.
- The newest 50 are kept (`[dev] checkpoints`).
- **Only `~/orion-dev` comes back.** Not the caches, not what an unsandboxed shell did elsewhere, not what went out over the network.

### What it doesn't protect against

- **Code you didn't write runs.** `npm install`, `pip install` and `cargo build` run their packages' scripts. With the network on, they can send out what the shell can read: in the sandboxed shell that's your projects and whatever else is under your readable roots, though not your keys; in the unsandboxed one, nearly everything you can read.
- **There's no list of allowed hosts.** The network is on or off.
- **The unsandboxed shell is best effort, not a wall.** It shuts the easy ways to root and out of the session (the helper, the daemon, the user bus, login scripts, other processes), but it runs as you. It can write to folders on your `PATH` such as `~/.local/bin` and `~/.cargo/bin`, so something it leaves there can run later, outside it. If you're in the `docker` group, it can use Docker, which is as good as root. Use it for a job, then `orion mode safe`.

## Snapshots and rollback

Every package change Orion makes (`pkg_install`, `pkg_remove`, `update_apply`) and all root work can be undone. Flatpak apps aren't part of this; remove one with `flatpak_remove`.

### Turn snapshots on

On Fedora or Ubuntu with btrfs, once:

```sh
sudo dnf install snapper            # or apt
sudo snapper -c root create-config /
```

openSUSE has this already.

### What Orion keeps

- **A history** of every change: each package's version before and after, dependencies and new kernels included, plus root work. It's in `~/.local/state/orion/packages.jsonl`.
- **A snapshot pair** around each change, where snapper is set up, described "orion update_apply" and so on. Snapper cleans old ones up.

### Undo a change

Just ask: "undo the last update." Or by hand:

```sh
orion call pkg_history                          # newest first: what changed, the snapshots, what was undone
orion call pkg_rollback '{"id": 12}'            # undo change 12
orion call pkg_rollback '{"id": 12, "method": "packages"}'   # by the package manager, even with snapshots
```

How it undoes:

- **With a snapshot pair,** `snapper undochange` puts back every file the change touched.
- **Without one,** it reinstalls the old versions (from the repositories, or pacman's cache) and removes what the change added. A version the repositories no longer carry can't come back this way; it says which before anything runs.

Rules:

- **Newest first.** A change can't be undone while a later one that touched the same packages is still in place. The refusal names it.
- **Root work goes back by its snapshots only,** and only while nothing later is in place, since which files it touched isn't written down.
- **Protected packages stay installed,** and a change is undone only once.
- **Reboot** after undoing a kernel or core libraries.

Settings: `[packages] snapshots = "required"` refuses package changes when no snapshot can be taken, and `"never"` turns them off. `[root] snapshots` does the same for root work, and is `"required"` by default.

## Private things

- **Private messages stay local.** Messages with words like password, bank or medical, things that look like keys, or private paths go only to a model on this machine. With no local model, Orion asks first.
- **Private files stay hidden from hosted models.** When Claude or Gemini is answering, a tool call that touches `~/.ssh`, `~/.gnupg`, `~/.aws` or another private path is refused.
- **Private notes and journal entries** are only shown to a model on this machine.
- **Programs the AI runs** can't see your keys or Orion's own files.

Add your own folders to `[chat.router] private_paths` (hosted models never see them) and `[sandbox] hide` (programs never see them).

## The audit log

Every tool call, by any model, agent or person, is one line in `~/.local/state/orion/audit.jsonl`.

```sh
orion audit -n 20         # the last 20
orion audit --verify      # check no log entry was changed, removed or added
```

Each entry carries a hash of the one before it, and the privileged helper keeps checkpoints where code running as you can't reach them, so tampering shows. [Operating Orion](operating.md#the-audit-log) explains each field.
