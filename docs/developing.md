# For developers

How the code is laid out, how to add a tool, and what's next.

## Build and test

You need Rust 1.95 or newer ([rustup](https://rustup.rs)) and a C compiler.

```sh
git clone https://github.com/Goliaith/Orion.git && cd Orion
cargo build             # or: cargo install --path .  (into ~/.cargo/bin)
cargo test              # about a minute
```

Larger end-to-end runs, which CI also runs:

| Script | What it checks |
|---|---|
| `tests/distros/run.sh` | the package and update tools, rollbacks and unattended updates on Fedora, openSUSE, Arch and Debian, in Docker |
| `tests/helper/run.sh` | the privileged helper, its snapshots and root work, as root |
| `tests/desktop/run.sh` | sound, screens and apps on a real headless desktop |
| `packaging/test.sh` | the release packages, installed in clean containers |

## How it fits together

```
   you ──▶ orion chat / shell / listen ──▶ Claude, Gemini, or a local model
              │  ▲ approval prompts
              ▼  │
   ┌────────────────────────────┐   MCP (stdio)   ┌──────────────────────┐
   │ orion                      │ ◀────────────── │ any MCP client/agent │
   │  policy → tool → audit log │                 └──────────────────────┘
   └─────────────┬──────────────┘
                 ▼
      /proc, /sys, files, programs   (root work through orion helper)
```

## Layout

```
src/
  lib.rs        Orion: the registry that applies the policy and audit to each call
  main.rs       the command line
  tool.rs       the Tool trait and risk levels
  policy.rs     the policy file, decisions, path scoping
  trust.rs      trust levels: what each writes, and which one a policy is nearest
  setup.rs      `orion setup`: connect a model, check the key, write the files, the trust level
  doctor.rs     `orion doctor`: check each part in order and offer fixes
  tuning.rs     self-tuning beyond voice: rate limits, slow transcription, approvals
  envfile.rs    ~/.config/orion/env, read by every command and the units
  audit.rs      the JSONL audit log, its hash chain and checkpoints
  mcp.rs        the MCP server (initialize, ping, tools/list, tools/call)
  daemon.rs     the background service: socket, peer checks, live reload, the stdio bridge
  approvals.rs  approvals through the daemon, and `orion approvals`
  remote.rs     the chat calling tools through the daemon
  helper.rs     the privileged helper: the root service, its client, who may use it
  sandbox.rs    sandboxing for proc_run: bubblewrap, Landlock, limits, cgroups
  seccomp.rs    the seccomp filter for sandboxed programs
  watch.rs      what needs attention: the checks, the daemon's watch, unattended updates
  interrupt.rs  Ctrl-C: the SIGINT flag, interruptible waits and reads
  tools/        the built-in tools, one module per area (system, fs, fileops, process,
                services, journal, timers, packages, flatpak, updates, changes
                (snapshots, history, rollback), root (root_run, sys_edit), network,
                netconf, disks, smart, sensors, gpu, models, audio, display, power,
                bluetooth, apps, clock, session); exec.rs runs programs for them
  chat/
    mod.rs      config, the Backend and Ui traits
    session.rs  one conversation across models: routing, catch-up, fallback, the tool loop
    quick.rs    the fast path: simple requests answered by one tool, no model
    cli.rs      `orion …` lines typed into the chat
    router.rs   the routing order, keyword rules and the private-path guard
    classifier.rs  the local model as routing judge
    claude.rs   the Claude backend (streaming Messages API)
    local.rs    the OpenAI-compatible backend (Ollama, llama.cpp, vLLM, LM Studio, Gemini)
    http.rs     shared HTTP: retries, rate-limit waits, proxies, SSE
    memory.rs   notes about each person
    journal.rs  the HelixCore journal: entries, decisions, recall
    store.rs    saved conversations
    terminal.rs the line-by-line chat, slash commands, `orion conversations`
    agent.rs    the conversation's own thread, shared by the full-screen chat, shell and listen
    listen.rs   hands-free voice (`orion listen`)
    machine.rs  live CPU, memory and GPU readings
    tui/        the full-screen chat
    shell/      the graphical shell: server, security, the page (shell.html)
  voice/        recording and pause detection (record.rs), speech to text (transcribe.rs),
                speech (speak.rs), the wake word (wake.rs), speaker verification
                (speaker.rs, neural.rs, torch.rs, enroll.rs), voice tuning (tuning.rs),
                `orion voice setup` (setup.rs)
docs/           the guides
packaging/      release files: build.sh, pkgbuild.sh, install.sh, assemble.sh, test.sh,
                and bootstrap.sh (install from the private repository)
contrib/        systemd units (daemon, helper, voice, shell, approvals), orion-kiosk,
                orion-wake (the wake-word detector)
tests/          end-to-end runs of the real binary
.github/        ci.yml (lint, tests, oldest Rust, distros, helper) and release.yml
```

## Adding a tool

1. Implement `Tool` in a module under `src/tools/`, and register it in `tools::builtin()`.
2. **Pick its `Risk` honestly** (`Read`, `Write` or `Execute`): the policy defaults and trust levels key off it.
3. Put checks no approval should override in `precheck`. They run before anyone is asked.
4. Put argument-dependent pre-approval (like `proc_run`'s command list) in `preapproval`.
5. If it needs root, build its command for the helper and check it again on the helper's side (`helper_check`).
6. Add it to [docs/tools.md](tools.md) and describe it in [docs/machine.md](machine.md).

## Making a release

See [Making a release](install.md#making-a-release).

## Roadmap

Done:

- **The interface:** typed tools, a policy, approvals and a tamper-evident audit log, over MCP.
- **The background service** with live reload, shared approvals and the privileged helper.
- **Isolation:** bubblewrap or Landlock, limits, a cgroup per program and a seccomp filter.
- **The machine:** services, packages, Flatpak apps, updates (unattended too) with snapshots and rollback, network, firewall, disks, GPUs and VRAM, the desktop, and root work beyond the tools.
- **Monitoring:** failed services, full disks, updates, reboots, SMART health, temperatures, battery wear, errors, hardware errors and failed logins.
- **Packaging:** release binaries, `.deb`, `.rpm`, a PKGBUILD, an install script and `bootstrap.sh`.
- **The front end:** the terminal chat, full-screen view and graphical shell; Claude, Gemini and local models with automatic routing; the fast path; saved conversations; per-person memory and the HelixCore journal.
- **Voice:** push to talk, spoken replies, the wake word and its detector, enrolled voices with a neural speaker model, hands-free mode, voice tuning and `orion voice setup`.
- **Setup:** `orion setup`, `orion doctor`, trust levels and self-tuning.

Next:

- GPU memory budgets per program, which cgroups can't express yet.
- A status-bar badge for pending security fixes.
- A voice benchmark on real recordings, and transcription on the GPU.
