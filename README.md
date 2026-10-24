# Orion

Orion is an operating system built around AI: Linux underneath, with a model as a first-class user of the machine.

This repository holds the first pieces:

- **The interface between the AI and the system.** A model never touches the machine directly. It talks to `orion`, which offers the machine as a small set of typed tools. A local policy decides what is allowed, and every call is written to an audit log.
- **The front-end**, `orion chat`. You talk to an AI in your terminal, and it works on the machine through those same tools. Each message goes to Claude or to a model running on this machine's own hardware, whichever fits: private and simple work stays local, complex work goes to Claude. Anything risky stops and asks you first.
- **The system service**, `orion daemon`: one policy, one audit log and one sandbox shared by every agent on the machine, on a Unix socket only you can reach.
- **A model of the machine**: services, packages, the network, the GPUs and the local models on them, so the AI can see what is running and manage it, within a VRAM budget for models.
- **The sandbox.** Every program the AI starts with `proc_run` runs confined: read-only outside its workspace, with your keys and secrets hidden, no network, no GPU, and limits on memory, CPU time and file size.

```
   you ──▶ orion chat ──▶ Claude (API)  or  a local model (Ollama, llama.cpp, vLLM, …)
              │  ▲ approval prompts
              ▼  │
   ┌────────────────────────────┐   MCP (stdio)   ┌──────────────────────┐
   │ orion                      │ ◀────────────── │ any MCP client/agent │
   │  policy → tool → audit log │                 └──────────────────────┘
   └─────────────┬──────────────┘
                 ▼
      /proc, /sys, files, programs
```

It speaks the [Model Context Protocol](https://modelcontextprotocol.io), so the same interface works for hosted models, local models and future in-house agents. Nothing in it is tied to one vendor.

**Guides:** [Installing Orion](docs/install.md) (building, models, optional parts, root access, services) and [Operating Orion](docs/operating.md) (day-to-day use, the policy, updates, the audit log, troubleshooting). This README describes each feature in depth.

## Tools

| Tool           | Risk    | Default | What it does |
|----------------|---------|---------|--------------|
| `system_info`  | read    | allow   | Hostname, OS, kernel, CPU, memory, load, uptime |
| `system_health` | read   | allow   | What needs attention: failed services, full or failing disks, security updates, a due reboot, low memory, heat, battery wear, journal errors, failed logins |
| `disk_health`  | read    | allow   | Each drive's SMART health: good, warning or failing and why, wear, temperature, hours of use |
| `sensor_status` | read   | allow   | Temperatures (CPU, GPU, drives, board) with their limits, and fan speeds |
| `gpu_info`     | read    | allow   | Every GPU (NVIDIA, AMD, Intel) by name: memory, load, temperature, power, compute stacks, and what runs on each, down to the model |
| `net_info`     | read    | allow   | Interfaces and addresses, default routes, DNS, and listening ports with the process behind each |
| `net_connections` | read | allow  | NetworkManager's devices and saved connections, marking those that carry an SSH session |
| `wifi_scan`    | read    | allow   | Wi-Fi networks in range, strongest first |
| `wifi_connect` | write   | ask     | Join a Wi-Fi network (the password never reaches logs or command lines) |
| `net_connection` | write | ask     | Bring a saved connection up or down, forget it, toggle auto-connect |
| `fw_status`    | read    | allow   | The firewall (ufw or firewalld): on or off, defaults, rules |
| `fw_rule`      | write   | ask     | Allow or deny a port, optionally from one network; never blocks SSH |
| `fw_enable`    | write   | ask     | Turn the firewall on or off; on only with SSH allowed |
| `disk_list`    | read    | allow   | Disks and partitions: model, size, filesystem, mounts, how full |
| `disk_usage`   | read    | allow   | What is using the space under a folder, biggest first |
| `disk_mount`   | write   | ask     | Mount a filesystem under /mnt or /media, with `nosuid,nodev` |
| `disk_unmount` | write   | ask     | Unmount (and eject) a filesystem; never the system's own |
| `fs_list`      | read    | allow   | Directory listing inside the readable roots |
| `fs_read`      | read    | allow   | Read a file, or a byte range of one, inside the readable roots |
| `fs_write`     | write   | ask     | Write or append to a file inside the writable roots |
| `fs_find`      | read    | allow   | Find files by name (wildcards) or by text they contain |
| `fs_mkdir`     | write   | ask     | Create a folder inside the writable roots |
| `fs_copy`      | write   | ask     | Copy a file or folder, within the writable roots |
| `fs_move`      | write   | ask     | Move or rename a file or folder, within the writable roots |
| `fs_delete`    | write   | ask     | Delete to the Trash (or for good), within the writable roots |
| `proc_list`    | read    | allow   | Processes, with filtering and sorting by memory |
| `proc_run`     | execute | ask     | Run one program (no shell) in the [sandbox](#sandbox), with a timeout and an output cap |
| `proc_signal`  | write   | ask     | Stop, interrupt, pause or resume a process; never the ones the system stands on |
| `svc_list`     | read    | allow   | systemd units: running, failed or stopped, and enabled at boot |
| `svc_status`   | read    | allow   | One unit's state, last result, memory, CPU, restarts and recent log |
| `svc_control`  | write   | ask     | Start, stop, restart, reload, enable or disable a unit (critical ones are protected) |
| `journal_read` | read    | allow   | The system journal, by unit, time, priority, boot or text |
| `timer_list`   | read    | allow   | systemd timers: what each runs, when it ran and runs next |
| `timer_create` | write   | ask     | Schedule a tool call, run through the policy and audit log when it fires |
| `timer_remove` | write   | ask     | Remove a timer Orion made |
| `pkg_list`     | read    | allow   | Installed packages (apt, dnf, zypper or pacman) |
| `pkg_search`   | read    | allow   | Search the repositories |
| `pkg_info`     | read    | allow   | One package: installed and available versions, description, dependencies |
| `pkg_install`  | execute | ask     | Install packages, as root and outside the sandbox |
| `pkg_remove`   | execute | ask     | Remove packages, refused if the system or the policy needs them |
| `update_check` | read    | allow   | Pending updates (security fixes and kernels marked), services to restart, reboot needed |
| `update_apply` | execute | ask     | Install updates: all, security fixes only, or named packages |
| `pkg_history`  | read    | allow   | Every package change Orion made: versions before and after, snapshots, rollbacks |
| `pkg_rollback` | execute | ask     | Put a change back, through its snapper snapshots or by reinstalling the old versions |
| `audio_status` | read    | allow   | Sound outputs and inputs (speakers, headphones, HDMI, Bluetooth, microphones): volume, mute, the default, and what plays |
| `audio_set`    | write   | allow\* | Set or nudge a volume, mute, or pick the default output or microphone |
| `display_status` | read  | allow   | Monitors (resolution, refresh rate, scale, position, on or off, modes) and the panel's brightness |
| `display_set`  | write   | ask\*   | Brightness (pre-approved), or a monitor's mode, scale, position, on or off; never the last screen off |
| `power_status` | read    | allow   | Batteries (charge, time left, health), the charger, the power profile, whether it can suspend |
| `power_set`    | write   | ask     | Switch the power profile: performance, balanced, power-saver |
| `power_action` | write   | ask\*   | Lock (pre-approved), suspend, hibernate, reboot or power off, through logind |
| `bt_status`    | read    | allow   | The Bluetooth adapter and its devices: paired, connected, battery; scans for new ones |
| `bt_device`    | write   | ask     | Turn Bluetooth on or off; pair, connect, disconnect or forget a device |
| `app_list`     | read    | allow   | The desktop apps installed (Flatpak and Snap included), and what they open |
| `app_open`     | execute | ask     | Start an app, optionally on files or links, or open them in their default apps |
| `clock_status` | read    | allow   | Local time, time zone, network time, the system locale and keyboard layout |
| `clock_set`    | write   | ask     | Set the time zone, network time, the system locale or keyboard layout |
| `model_status` | read    | allow   | Local inference servers, their models (available and loaded), and the VRAM budget |
| `model_load`   | write   | ask     | Load a model on Ollama, within the VRAM budget (optionally unloading others) |
| `model_unload` | write   | ask     | Unload a model, freeing its VRAM |
| `model_pull`   | write   | ask     | Download a model onto Ollama |
| `gpu_assign`   | write   | ask     | Pin an inference server (a systemd service) to chosen GPUs |

Out of the box the AI can look at the machine freely. Changing it (writing files, running programs, managing services, packages and models) needs your approval each time. With no human present (`orion serve`), "ask" means no. \* Turning the volume up or down, the panel's brightness and locking the screen are pre-approved by `[desktop] approved`, because they are harmless and easily undone; the rest of those tools ask.

## Install

Linux only, because the tools read `/proc` and `/sys` directly. Each [release](https://github.com/Goliaith/Orion/releases) has binaries for x86_64 and aarch64 that run on Debian 11, Ubuntu 20.04, RHEL 9 and anything newer, as a `.deb`, an `.rpm`, an Arch PKGBUILD, and a tarball with an install script:

```sh
curl -fsSL https://github.com/Goliaith/Orion/releases/latest/download/install.sh | sudo sh   # or without sudo: just you, in ~/.local
orion tools                         # every tool and what the policy decides
export ANTHROPIC_API_KEY=sk-ant-... # and/or run a local model with Ollama
orion chat
```

From the private repository, with the GitHub CLI signed in (`gh auth login`), one command installs the build tools, Rust, Orion, its root helper and its daemon, and running it again updates them ([details](docs/install.md#from-the-private-repository-in-one-command)):

```sh
gh api -H 'Accept: application/vnd.github.raw' repos/Goliaith/Orion/contents/packaging/bootstrap.sh | sh
```

To build from source by hand instead, you need Rust 1.95 or newer ([rustup](https://rustup.rs)) and a C compiler:

```sh
git clone https://github.com/Goliaith/Orion.git && cd Orion
cargo install --path .              # installs ~/.cargo/bin/orion
```

[Installing Orion](docs/install.md) covers each step in full: the packages for each distribution, local models, the sandbox, voice, root access for the machine tools, and running Orion as a service. `cargo test` runs the tests.

## Chat

```sh
export ANTHROPIC_API_KEY=sk-ant-...    # from console.anthropic.com
orion chat
```

```
Orion
  claude  claude-opus-5-5 · effort high
  local   qwen3:8b · local at http://127.0.0.1:11434/v1
routing: auto (private → local; qwen3:8b judges the rest) · 7 tools (5 automatic, 2 ask first) · /help · Shift-Tab switches routing

orion auto › what's eating my memory? write a short report

I'll check the hardware and the biggest processes.
  ⚙ system_info
    ✓ 0ms
  ⚙ proc_list {"limit":5,"sort":"memory"}
    ✓ 3ms
  ⚙ fs_write {"content":"…","path":"~/orion-workspace/memory.txt"}
  ? Approve fs_write?
    { "content": "…", "path": "~/orion-workspace/memory.txt" }
  [y]es / [n]o / [a]lways for this session › y
    ✓ 1ms
Firefox is using 3.1 GB of your 16 GB. I wrote the details to ~/orion-workspace/memory.txt.
```

- **Streaming.** Replies stream as they are written. The model's progress notes and reasoning show dimmed, so long tasks don't sit silent.
- **Every tool call is visible**, with its arguments and whether it worked.
- **Approval prompts** show the full arguments of anything that changes the machine. `a` approves that tool for the rest of the session.
- **Commands:** `/use`, `/route`, `/model`, `/models`, `/effort`, `/tools`, `/usage`, `/voice`, `/memory`, `/keys`, `/new`, `/history`, `/resume`, `/title`, `/exit`, plus `@claude` / `@local` prefixes and the [hotkeys](#hotkeys). Line history is kept in `~/.local/state/orion/chat_history`.
- Settings live under `[chat]` in the policy file: the mode (`provider = "auto"`, `"claude"` or `"local"`) and the sections `[chat.claude]` (model, default `claude-opus-5-5`; effort; progress notes; fallbacks), `[chat.local]` and `[chat.router]`.

`ANTHROPIC_AUTH_TOKEN` (an OAuth access token) also works in place of an API key.

### Graphical shell

```sh
orion shell            # prints an address like http://127.0.0.1:7310/?token=…
orion shell --open     # and opens it in your browser
```

![The Orion shell: conversations on the left, the chat with routing, tool calls and an approval card in the middle, the machine panel on the right](docs/shell.png)

`orion shell` serves Orion's graphical interface from this machine. It has the same conversation, routing, approvals and tools as the terminal chat, plus:

- **A sidebar of saved conversations.** Click one to continue it, or start a new one.
- **Routing at a glance:** an Auto / Claude / Local switch in the header (Alt+M cycles it), and every reply labeled with the model that wrote it and why.
- **Live tool activity:** each tool call shows a spinner, then its time or the error. Reasoning is collapsible (the bulb button, or Alt+R).
- **Approval cards** with Approve, Always allow this session, and Decline.
- **A machine panel:** CPU with a sparkline, memory, each NVIDIA GPU's load, VRAM and temperature, the connected models, and token use.
- **Voice:** the mic button (Alt+T) listens with a level meter and puts the transcript in the box. The speaker button reads replies aloud.
- **Shared across tabs.** Every open tab or window shows the same live conversation, and one that opens late, or reloads, is caught up.
- **Keyboard and layout:** Enter sends, Shift+Enter starts a new line, and Esc stops the AI. It follows the system's light or dark theme. On narrow screens the panels become slide-over drawers.

**Make it the whole desktop.** On a machine dedicated to Orion, boot straight into it with a kiosk Wayland compositor:

```sh
cage -- contrib/orion-kiosk      # starts `orion shell`, then a full-screen browser on it
```

[`contrib/orion-shell.service`](contrib/orion-shell.service) runs the shell as a systemd user service instead.

**Security.** A local web server is something other users and web pages could try to reach, so the shell is locked down:

- **It listens on `127.0.0.1` only.** For another machine, use an SSH tunnel.
- **Every run gets a random 128-bit token.** The printed address carries it, and on first load it is swapped for an `HttpOnly`, `SameSite=Strict` cookie and removed from the address bar. The address is also saved, readable only by you, in `~/.local/state/orion/shell-url`.
- **Requests must name this server in `Host`,** which defeats DNS-rebinding attacks.
- **Anything that changes state needs an `X-Orion` header.** A cross-site page can only send that after a CORS preflight, and the shell never approves one.
- **The page loads nothing from outside,** and a strict Content-Security-Policy enforces it. Model output is HTML-escaped before it is rendered as Markdown.

### Full-screen interface

On a terminal, `orion chat` opens full screen:

```
┌ Orion · why is my machine slow? save a note ───────────────────────────────────┐┌ Machine ─────────────────────────┐
│you     why is my machine slow? save a note                                     ││ vm                               │
│        → Claude (claude-opus-5-5) · qwen3:8b: needs root-cause reasoning (0.3s)││ Ubuntu 24.04.5 LTS               │
│        I'll check the hardware and what's using memory.                        ││ CPU  ██░░░░░░░░  18% ·4c         │
│        ⚙ system_info   ✓ 0ms                                                   ││ RAM  ██████░░░░ 9.1/16G          │
│        ⚙ proc_list {"limit":5,"sort":"memory"}  ✓ 4ms                          ││ GPU0 RTX 4070                    │
│        ⚙ fs_write {"content":"4 cores, 16 GB RAM\n","create_dirs"…  ✓ 0ms      ││  use ████░░░░░░  42% 61°C        │
│        ┌─ Approve fs_write?                                                    ││  mem ███░░░░░░░ 3.1/12G          │
│        │ { "path": "~/orion-workspace/notes.txt", … }                          ││ Routing                          │
│        └─ approved                                                             ││ auto (private → local; qwen3:8b  │
│claude  This is a 4-core Xeon VM with 16 GB of RAM, mostly idle. I saved a note ││ judges the rest)                 │
│        to ~/orion-workspace/notes.txt.                                         ││ claude claude-opus-5-5 · effor…  │
│                                                                                ││ local  qwen3:8b · local at htt…  │
│                                                                                ││ Session                          │
│                                                                                ││ claude in 6.2k · out 150         │
└────────────────────────────────────────────────────────────────────────────────┘└──────────────────────────────────┘
┌ message · Enter sends · Alt-Enter new line ────────────────────────────────────────────────────────────────────────┐
│›                                                                                                                   │
└────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
 AUTO   Shift-Tab routing · Ctrl-O reasoning · Ctrl-B panel · PgUp/PgDn scroll · /keys · Ctrl-D quit
```

- **The conversation:** your messages, routing decisions, reasoning (dimmed), every tool call with a live status (`…`, then `✓ 12ms` or `✗` with the error), and replies. Code blocks are highlighted.
- **Approvals appear as cards** in the conversation, and you answer them with `y`, `n` or `a` (always, for this session).
- **The machine panel** (Ctrl-B) shows live CPU and RAM, each NVIDIA GPU's load, VRAM and temperature, which models are connected and how routing works, and token use per model. It reads the machine directly, so polling never fills the audit log.
- **The screen stays live while the AI works.** The conversation runs on its own thread, so you can scroll, read, or type your next message ahead. It sends when the AI is done.
- **The status bar** shows the routing mode, a spinner while working, and the key hints.
- **Keys:** the [hotkeys](#hotkeys) below work the same here. Also Ctrl-B (the panel), PgUp/PgDn or the mouse wheel (scroll; End jumps back to the latest), and Ctrl-J or Alt-Enter (new line). Pasted text keeps its line breaks. The mouse is captured for scrolling; hold Shift to select text in most terminals.
- **Resuming** shows the whole earlier conversation above a "resumed" divider.

`orion chat --plain`, or `interface = "plain"` under `[chat]`, keeps the line-by-line chat, and it's used automatically when input or output isn't a terminal. Both interfaces share the same session, commands and keys.

### Voice

Press **Ctrl-T** and talk. A live level meter shows Orion is listening. When you pause, the recording ends (or press Ctrl-T again full screen, Enter in the line chat; Esc or Ctrl-C cancels). Your words land in the message box to check and edit before you press Enter.

```
┌ ● listening ▁▁▁▆▆▆▆▆▆▅▅▅ 1s · pause or Ctrl-T to finish · Esc cancels ───────────┐
│›                                                                                 │
└──────────────────────────────────────────────────────────────────────────────────┘
┌ message · Enter sends · Alt-Enter new line ──────────────────────────────────────┐
│› how full is my disk?                                                            │
└──────────────────────────────────────────────────────────────────────────────────┘
```

`/voice speak on` makes Orion **speak its replies**, sentence by sentence as they stream in. Code blocks are left on screen ("code shown on screen"), and Markdown symbols are not read out. Ctrl-C, or starting your next message, stops it mid-sentence. `/voice` shows what is set up, and `/voice test` says a test sentence.

It is local-first and uses what the machine already has:

| Part | Default | Alternatives |
|---|---|---|
| Microphone | the first of `parecord` (PulseAudio/PipeWire), `arecord` (ALSA), `sox`, `ffmpeg` | any command writing 16 kHz mono 16-bit PCM to stdout (`record_command`) |
| Speech to text | an OpenAI-compatible server at `http://127.0.0.1:8000/v1`, such as [speaches](https://github.com/speaches-ai/speaches) (faster-whisper), LocalAI, or whisper.cpp's server | a command that prints the transcript, for example `whisper-cli … -f {wav}` (`transcribe_command`) |
| Text to speech | `espeak-ng`, `espeak` or `spd-say` | [piper](https://github.com/rhasspy/piper) as a command, or an OpenAI-compatible `/audio/speech` server such as Kokoro-FastAPI (`speak_url`) |

- **Listening ends on a pause.** Orion measures the room's noise floor at the start and waits for at least 0.3 s of speech before a pause can end the recording, so a click doesn't cut you off.
- **Transcripts are cleaned up.** Whisper's `[BLANK_AUDIO]`-style markers are removed.
- **A spoken message is routed like a typed one,** so private speech stays on the local model.
- **Nothing is sent off the machine** unless you point `transcribe_url` or `speak_url` at a remote server.
- **Settings are under `[voice]`.** Set `send_after_transcribe = true` to send without the editing step.

#### Wake word

Turn on wake listening with **Alt-W** (or `/voice wake on`, or `wake = true` under `[voice]`), and talk to Orion hands-free in the full-screen chat or the shell:

- **"Orion, what's using the GPU?"** is sent at once, as if you had typed it.
- **A bare "Orion"** plays a short chime, and the next thing you say is the request: `● listening · say your request`.
- **"Hey Orion", "OK Orion"** work too. The phrase only counts at the start of what you say, so "I read about Orion" doesn't wake it.
- **Spoken requests act like typed ones.** While the AI is working, they wait their turn, and "Orion, stop" interrupts it.
- **Spoken "no" declines an approval.** A spoken "yes" approves only with `wake_approvals = true`, because anything playing audio nearby could say it.

How it works: the microphone stays open, and a pause detector (with a noise floor that follows the room) cuts the sound into utterances. The first 3 seconds of each utterance go to your transcriber and are checked for the wake phrase. The check allows for the ways a transcriber spells a name it doesn't know ("O'Ryan", "Orien", "Oreon"), and `wake_aliases` adds your own. Utterances are only held in memory until they are checked, and nothing is saved.

- **It stays local.** Wake listening refuses to start if your transcriber is on another machine, since everything said near the microphone would go there. Set `wake_allow_remote = true` if that is really what you want.
- **A dedicated detector:** set `wake_command` to a program that prints a line each time it hears the wake word, such as an [openWakeWord](https://github.com/dscripka/openWakeWord) or Porcupine script. It does the always-on listening, so nothing is transcribed until it fires, and Orion then records and transcribes the request.
- **The cost:** transcribing every utterance is light with a GPU or a small Whisper model, but a dedicated detector uses far less power on a CPU.
- **Your own phrase:** `wake_phrase` sets it, for example `"hey computer"`. A short, distinctive name works best.
- **No leftover microphone:** the microphone is closed as soon as wake listening stops, and it can't outlive Orion even if Orion is killed.
- **The line-by-line chat** can't take input while it waits at its prompt, so wake listening is only in the full-screen chat, the shell and [headless voice](#headless-voice).

#### Speaker verification

Enroll your voice, and the voices of the people you live or work with, and only you can wake Orion. Everyone else, and the television, is ignored. Orion knows who is asking.

```sh
orion voice model install       # the neural speaker model: about 16 MB from PyPI, once
orion voice enroll              # you: six short phrases, about a minute
orion voice enroll --name ana   # and anyone else
orion voice test                # say anything: "That's Ana: similarity 0.90 (needs at least 0.80)"
orion voice status              # who is enrolled, and what each may do
orion voice forget ana          # or --all: anyone can wake Orion again
```

- **It applies wherever Orion listens for its wake word:** the full-screen chat, the shell and `orion listen`. It covers spoken approvals too, so "Orion, yes" from someone else approves nothing. Ctrl-T recordings aren't checked, since you're at the keyboard.
- **Other voices are never transcribed.** Each utterance is checked before anything transcribes it, and one in another voice is dropped. Their words never reach the transcriber, and `/voice` shows how many were ignored.
- **A bare "Orion" is often too short to judge**, so it may wake Orion, but the request that follows is judged together with it and must match. If it doesn't, the chat and the shell show "that didn't sound like your voice", and `orion listen` says "Sorry, I didn't recognize your voice."

**Several people.** Each utterance is matched against everyone enrolled, and the closest person is taken if they are close enough.

- **The AI is told who is asking:** "Ana (by voice): what's using the GPU?". The chat and the shell show the same.
- **The audit log names them:** `"caller": "ana (by voice)"` on every tool call their request makes.
- **What each person may do** is set under `[voice.people.<name>]` in the policy:

  ```toml
  [voice.people.kid]
  tools = "read"      # "all" (default), "read" (only tools that look at the machine), or "none" (just talk)
  approve = false     # may not answer approval prompts by voice (default true, with wake_approvals)
  ```

  A tool their request would need is refused before anyone is asked, whatever the policy otherwise allows. The AI is told why, and the refusal is in the audit log. Someone who may not approve hears "Sorry, Kid, you can't approve that," and the question stays open for someone who can.
- **Enrolling warns** when a new person's phrases also match someone already enrolled, since Orion may mix the two up.
- **The single voiceprint of earlier versions** moves to `~/.local/state/orion/voices/<login name>.json` the first time it's needed.

There are two methods, and Orion uses the better one it has (`verify_method = "auto"`).

**The neural speaker model (recommended).** This is the GE2E speaker encoder (Wan et al., 2018) with the pretrained weights published as [Resemblyzer](https://github.com/resemble-ai/Resemblyzer) (Apache-2.0), trained on thousands of real speakers. Each utterance becomes a 256-number voice embedding, and it is compared with the average of your enrollment phrases by cosine similarity. It passes at 0.80 or more (`verify_similarity`, higher is stricter).

- **It runs here, in plain Rust:** a mel spectrogram exactly as librosa computes it, three LSTM layers and a linear layer, about 0.1 s for a 3-second utterance. Tests hold it to the Python reference implementation: the same features, and the same embedding to a cosine similarity above 0.999.
- **Installing:** `orion voice model install` downloads the Resemblyzer 0.1.4 wheel from PyPI and checks it against a pinned SHA-256. It reads the PyTorch weights with a small reader of its own that refuses anything but tensors, so unpickling can't run code. It saves a 5.7 MB model to `~/.local/share/orion/models/`. Use `--from <wheel or pretrained.pt>` on a machine without internet. Enroll again after installing it, since a voiceprint made without the model has no embedding.
- **How well it works:** on a real two-person conversation (pyannote's sample recording), enrolled on one speaker, their own held-out speech scored 0.84 and 0.90. The other speaker's turns scored 0.55 to 0.84, and one of eight passed the 0.80 limit. That's a small, noisy test (turns from a meeting, with crosstalk), not a benchmark. It does not like synthesized voices: espeak-ng's robotic voices all sound alike to it.

**The built-in method (no download).** Each 10 ms of speech becomes 19 mel-cepstral coefficients and their deltas. Enrollment summarizes them as a codebook of 32 typical frames, and an utterance is scored by how far its frames lie from it, relative to your own phrases (about 1 for you; the limit is 1.2, `verify_threshold`, lower is stricter).

- **It tells microphones and rooms apart better than people.** On 8 espeak-ng voices it rejected 0 of 48 of the enrolled voice's phrases and accepted 4 of 648 from others. But on the real conversation it let the other speaker in, while it rejected a recording from elsewhere. It keeps out the television and other devices, not someone talking in the same room. Install the neural model for that.

For both:

- **It keeps other voices out; it isn't proof of identity.** A recording of you gets through, and so may a similar voice, which is why spoken approval stays off unless you turn on `wake_approvals`.
- **Enroll where you usually talk to Orion,** with the same microphone, and enroll again if that changes. Enrollment warns about a phrase that doesn't match the rest.
- **Voiceprints** are in `~/.local/state/orion/voices/`, one file per person, readable only by you. It holds averaged features that can't be turned back into speech. Programs the AI runs can't see it; the sandbox hides Orion's whole state directory.
- **Settings** under `[voice]`:
  - `verify_speaker = "auto"`: on once a voice is enrolled. `"on"` refuses to listen without one, and `"off"` never checks.
  - `verify_method = "auto"`, `"neural"` or `"builtin"`.
  - `verify_similarity` and `verify_threshold`.

#### Headless voice

`orion listen` is Orion with no screen at all: it listens for the wake phrase, works on what you ask, and speaks the answer. That makes it a voice assistant for the machine, made to run as a service.

```sh
orion listen                        # until stopped; --local / --claude / --auto, --resume
systemctl --user enable --now orion-voice.service    # or as a user service (contrib/)
journalctl --user -u orion-voice -f                  # what it heard and said
```

```
Orion is listening: say "orion" (heard by Systran/faster-whisper-small at http://127.0.0.1:8000/v1)
approvals by voice: on ("Orion, yes")
you: how long has this machine been up?
⚙ proc_run {"command":"uptime"}
asking: May I run uptime?
you: yes. (approved)
  ✓ 22ms
orion: The machine says: up 35 min, load average: 0.45, 0.25, 0.21.
```

- **The same conversation engine.** Routing, tools, the policy, the sandbox, the audit log and saved conversations all work as in the chat. A new conversation starts when the last request is more than 10 minutes old (`listen_new_after_mins`).
- **Approvals are asked aloud.** With `wake_approvals = true`, Orion asks "May I restart ollama? Say Orion yes, or Orion no." and waits 30 seconds for an answer; silence or anything else counts as no. With it off (the default), tools that need approval are declined, and Orion says so. Pre-approve what you want it to do freely in the policy (`[tools]`, `proc.allowed_commands`, `services.allowed`, …).
- **Hands-free control:** "Orion, stop" interrupts the AI. A request made while it is working waits its turn ("I'll do that next."). Errors are spoken briefly and logged in full.
- **It needs all three parts:** a microphone, speech to text and speech. It names whatever is missing at startup. If the microphone fails later, it tries again every 10 seconds.
- **Only one listener:** run either this or wake listening in a chat or shell, not both, or both will answer.

### Saved conversations

Every conversation is saved after each message, so closing the terminal (or a crash) loses nothing:

```sh
orion chat --resume               # continue the most recent conversation
orion chat --resume 20261020-15   # or a specific one, by id or id prefix
orion conversations               # list them: id, last active, length, title
orion conversations show <id>     # print a transcript
orion conversations delete <id>
```

```
Resumed “Cleanup plan” · 3 messages
  you: second message
  claude: Reply 1.
  you: third message
  claude: Reply 2.
```

Inside a chat:

- `/history` lists saved conversations, most recent first.
- `/resume <n|id>` switches to one; the current conversation is already saved.
- `/new` starts a fresh conversation.
- `/title <text>` renames the current one. Titles otherwise come from the first message, and a conversation that starts with a private message is titled "(private conversation)".

How it works:

- **Each model's own history is saved exactly as it was produced,** alongside the shared transcript. Claude's thinking blocks and their signatures are included, so a resumed conversation continues with full context (and prompt caching), not from a summary. Each model's id is saved too, and comes back on resume unless you pass `--model`.
- **The routing mode comes back as well.** If that mode's model is gone (say the local server is off), the conversation continues in `auto`. A model that isn't connected when you resume keeps its saved history in the file, and gets it back as soon as it connects (with `/use` or Shift-Tab).
- **Files live in `~/.local/state/orion/conversations/`** (`$XDG_STATE_HOME`), one JSON file per conversation. They are readable only by you (the folder is `0700`, the files `0600`) and written atomically. They hold private turns too, which never leave the machine. Set `save_conversations = false` under `[chat]` to stop saving.

### People and memory

Each person has their own conversations, and Orion remembers things about each of them across conversations.

- **Whose conversation it is:** a spoken request from someone [enrolled](#speaker-verification) goes to *their* conversation. Their latest one continues if it was active in the last 10 minutes (`voice.listen_new_after_mins`); otherwise a new one starts. Typed messages belong to whoever is logged in. When the speaker changes, Orion saves the current conversation and switches, and says so ("Ana's conversation").
- **Memory:** the AI has two tools, `memory_save` and `memory_forget`, and uses them for lasting facts: a preference, a project and where it lives, how someone likes things done. Each conversation starts with what is remembered about that person ("You are talking with Ana. What you remember about Ana: …"), and only that person.

  ```
  bea: remember that I prefer metric units.
  ⚙ memory_save {"text":"Prefers metric units"}
  orion: Got it, Bea: I'll remember that.
  carl: what do you know about me?
  a new conversation with Carl
  orion: Carl, I don't know anything about you yet.
  bea: what do you know about me?
  Bea's conversation
  orion: Bea, I remember: Prefers metric units.
  ```

- **Secrets are never kept.** A note that looks like a password, key or token is refused.
- **Private notes stay local.** A note the router would call private (health, money, private paths, …) is only given to the local model; Claude is told only that some notes are withheld.
- **Limits:** at most 60 notes of 300 characters each per person, so memory stays small enough to send with every conversation.
- **The memory part of the prompt is fixed for a conversation,** so saving a note doesn't upset Claude's prompt caching. The model knows about the new note from the tool result.
- **You're in control:**
  - `/memory` lists what is remembered about whoever the conversation is with, and `/memory forget <id>` or `/memory clear` removes notes.
  - From the shell prompt: `orion memory [--person ana] [forget <id> | clear]`.
  - Memory tool calls are in the audit log, with the speaker as caller.
- **Where it's kept:** each person's memory is one file in `~/.local/state/orion/memory/`, readable only by you. Programs the AI runs can't see it. Set `memory = false` under `[chat]` to turn memory off.
- **On screen:** the full-screen chat and the shell show whose conversation is open and how much is remembered. `/history` and `orion conversations` show whose each conversation is.

### Hotkeys

| Key | What it does |
|---|---|
| **Ctrl-C** | Stops the AI mid-turn: a streaming reply, a long think, a running program or an approval prompt. Any partial reply is kept and marked as interrupted, so the model knows where it stopped. At the prompt, press it twice to quit. |
| **Shift-Tab** | Cycles routing between auto, claude and local, keeping what you've typed. Models that can't be reached are skipped. |
| **Ctrl-T** | Talks: listens until you pause, then puts what you said in the message box. See [Voice](#voice). |
| **Alt-W** | Turns [wake-word listening](#wake-word) on or off (full screen and the shell). |
| **Ctrl-O** | Shows or hides reasoning and progress notes (default: `show_reasoning` under `[chat]`). |
| **Alt-Enter** | Starts a new line, for multi-line messages. Pasting multi-line text also works. |
| **Ctrl-R**, **↑ ↓** | Search and browse your earlier messages. |
| **Ctrl-L** / **Ctrl-D** | Clear the screen / quit. |

The prompt shows the current routing (`orion auto ›`), and `/keys` lists the hotkeys. What Ctrl-C does underneath:

- **Network calls run on a worker thread.** The chat walks away from them, and the connection is dropped, which stops generation on the server rather than letting it run on in the background.
- **Programs started by the AI are killed** along with their whole process group.
- **An open approval prompt counts as "no".**
- **Every tool call the model already made still gets an answer** ("interrupted by the user"), so the conversation stays valid for the next message.
- **An interrupt is never treated as a failure,** so it doesn't trigger a fallback to the other model.
- **A second Ctrl-C** before the first is handled quits Orion outright, as an escape hatch if something hangs.

### Automatic routing

With both Claude and a local model available, `orion chat` (mode `auto`, the default) picks a model for every message and says which one it chose and why:

```
orion auto › how full is my disk?
→ the local model (qwen3:8b) · short request
  ⚙ proc_run {"args":["-h","/"],"command":"df"}
Your root disk is 28% full.

orion auto › plan a cleanup of my home directory
→ Claude (claude-opus-5-5) · complex: "plan"
Plan: 1) clear ~/.cache (2.1 GB), 2) empty Trash, 3) prune old Downloads. Start with the cache?

orion auto › yes
→ Claude (claude-opus-5-5) · follow-up

orion auto › my bank password is hunter2, remember it
→ the local model (qwen3:8b) · private: mentions "password"
```

How it decides, in order:

1. **Private stays local.** A message is private if it contains a private word (password, bank, medical, …), something shaped like a key or token, or one of the `private_paths`. If no local model is available, Orion asks before sending it to Claude. This is always a rule; no model can overrule it.
2. **Short follow-ups** ("yes", "ok, go ahead", "thanks") stay with the model that answered last.
3. **Everything else is judged on difficulty by the local model.** It is given a description of both options and the previous message, and answers `{"route": "claude"|"local", "reason": "…"}`. Its reason and how long it took are shown (`→ Claude (claude-opus-5-5) · qwen3:8b: multi-step migration planning (0.3s)`). When unsure it picks `prefer` (local by default).
4. **If no local model is running, or the judge fails or times out,** keyword rules decide instead. They send code, complex words (debug, refactor, plan, explain, write, …), several questions or numbered steps, and long messages to Claude, and everything else to `prefer`. After three failures in a row the judge is switched off for the session. Set `strategy = "rules"` to always use the rules; they're instant, and cruder.
5. **If the chosen model is down, or fails before replying, the other one answers.** Private messages never fall back to Claude.

The judge runs on this machine, so a message is never sent to Claude just to decide where it should go, and private messages never reach the judge at all. A small, fast model works well as the judge (`classifier_model = "qwen3:1.7b"`) while a bigger one handles the chat. Orion asks for JSON mode and no reasoning pass (`chat_template_kwargs`), and if the server rejects those fields it retries without them, and remembers.

**One conversation, two models.** Before a model answers, it is caught up on the turns the other model handled: their messages, replies and tool calls. Switching never loses context. **Private turns are never shown to Claude**, now or later.

**Claude never sees private files.** When Claude is answering, any tool call that refers to a `private_paths` entry (`~/.ssh`, `~/.gnupg`, `~/.aws`, …) is refused before it runs, whatever the policy allows. The check catches relative paths and symlinks too. The local model is not restricted this way.

Control it with:

- `@claude …` or `@local …` sends one message to that model.
- `/use auto|claude|local` switches mode; the conversation carries over.
- `/route <text>` shows where a message would go. Only the local judge sees it.
- `orion chat --auto`, `--claude` and `--local` choose the starting mode.
- The rules (word lists, private paths, `prefer`, length threshold) are configured under `[chat.router]`; see [`orion.example.toml`](orion.example.toml).

### Local models

Orion can use a model running on your own GPU instead of Claude. Nothing leaves the machine, and no API key is needed. It speaks the OpenAI-compatible chat API (`/v1/chat/completions`, with streaming and tool calls), which every major local runtime serves:

| Runtime | Start it | `[chat.local] base_url` |
|---|---|---|
| [Ollama](https://ollama.com) | `ollama pull qwen3:8b` (the server runs as a service) | `http://127.0.0.1:11434/v1` (default) |
| llama.cpp | `llama-server -m model.gguf --jinja` | `http://127.0.0.1:8080/v1` |
| vLLM | `vllm serve Qwen/Qwen3-8B --enable-auto-tool-choice --tool-call-parser hermes` | `http://127.0.0.1:8000/v1` |
| LM Studio | start the local server in the app | `http://127.0.0.1:1234/v1` |

```sh
orion chat --local                    # only the local model: first one the server lists
orion chat --local --model qwen3:8b   # a specific one
```

Inside a chat, `/models` lists what the server has, and `/model local <id>` switches model.

Things to know:

- **Pick a model trained for tool calling.** Qwen3, Llama 3.1+, Mistral Small 3 and gpt-oss work well. Smaller models (under ~7B) often call tools badly or not at all.
- **Context length matters.** Orion's tool definitions plus the results of a few calls fill a small window fast. Ollama's default context is short; raise it with `OLLAMA_CONTEXT_LENGTH=32768`, or with `-c` on llama-server.
- **Reasoning display.** Models that reason (Qwen3, DeepSeek-R1, gpt-oss) show their reasoning dimmed, whether the server sends it separately (`reasoning_content` / `reasoning`) or inline in `<think>` tags. It is not sent back to the model in the history.
- **The same policy applies.** A local model gets exactly the tools, approvals and audit log that Claude does.

## Other commands

```sh
orion tools                                   # every tool, its risk, and the policy decision
orion call system_info                        # call a tool yourself, exactly as the AI would (prompts for "ask" tools)
orion call fs_list '{"path": "~"}'
orion audit -n 20                             # recent calls, as JSON lines
orion audit --verify                          # check no entry was changed, removed or inserted
orion conversations                           # saved chats (see "Saved conversations")
orion memory                                  # what the AI remembers (see "People and memory")
orion shell                                   # the graphical shell (see "Graphical shell")
orion listen                                  # headless voice: "Orion, …" (see "Headless voice")
orion voice model install                     # the neural speaker model (see "Speaker verification")
orion voice enroll                            # only your voice wakes Orion
orion serve                                   # MCP server on stdin/stdout (through the daemon when it runs)
orion daemon                                  # the system service (see "System service")
orion status                                  # whether the daemon runs, and how
```

### Connect a model

Any MCP client can launch `orion serve`. For Claude Code:

```sh
claude mcp add orion -- /path/to/orion serve
```

Or in a client's JSON config:

```json
{ "mcpServers": { "orion": { "command": "/path/to/orion", "args": ["serve"] } } }
```

## The machine

Beyond files and processes, the AI sees and manages the parts of the machine an AI-first system cares about.

### Watching the machine

Orion notices problems before anyone asks. The daemon checks every 15 minutes:

| Check | Needs attention when |
|---|---|
| Services | a system service, or one of yours, has failed |
| Disks | a writable filesystem is 90% full (`disk_percent`) |
| Updates | security updates are pending (listed every 6 hours) |
| Reboot | the kernel was replaced, the package manager asks for one, or PID 1 runs replaced libraries |
| Memory | under 5% is available (`memory_percent`) |
| Disk health | a drive's SMART data says it is failing, or shows bad, pending or unreadable sectors, uncorrectable errors, or 90% of an SSD's life used (read every 6 hours; needs smartmontools, and root through the helper) |
| Temperatures | a CPU, GPU, drive or board sensor is within 10 °C of its own critical limit, or past a default for its kind where it has none (90 °C for a CPU, 70 °C for a drive); critical within 3 °C |
| Battery | it holds under 70% of its charge when new (`battery_health_percent`) |
| Errors | one program or unit logs 50 errors in an interval (`journal_errors`) |
| Hardware errors | the kernel reports any CPU or memory hardware error, filesystem error, disk I/O error, GPU crash or reset, thermal throttling, or ran out of memory and ended a program |
| Logins | 10 failed logins in the last hour, by SSH, sudo, su or the login screen, with the address most came from (`failed_logins`) |

- **A desktop notification** comes the first time each problem appears, not again while it lasts, and again if it comes back after clearing.
- **`orion status`** lists what needs attention, with what to do about each. Without the daemon it runs the quick checks itself (all but updates).
- **The AI knows:** each conversation starts with the findings, and the AI mentions them once when it fits ("by the way, /home is 93% full") and offers to help, without acting unasked. `system_health` checks afresh.
- **The interfaces show them:** the line-by-line chat under its header, the full-screen chat's panel and the shell's machine panel.
- **Settings** are under `[watch]`: the interval, the thresholds, notifications, and `ignore` for findings you don't care about, by key (`"service:backup.service"`, `"disk:/mnt/archive"`, `"reboot"`, `"temp:*"`, `"errors:noisy.service"`).
- **The system journal** is readable only to members of `systemd-journal` (or `adm` or `wheel`, by distribution). Without that, the error and login checks see only your own programs: `sudo usermod -aG systemd-journal "$USER"`.
- **`disk_health`** and **`sensor_status`** show the details behind the disk-health and temperature findings: each drive's state, wear, temperature and hours of use; every temperature with its limits, and fan speeds.

### The desktop

The desktop tools act on the session of the person logged in: its sound server, compositor and session bus. The daemon runs as a systemd user service, which doesn't inherit those, so Orion finds them in the user manager's environment (desktops put them there) or by their sockets in the runtime directory.

- **Sound** goes through `pactl`, which PipeWire and PulseAudio both speak (version 16 or newer, for its JSON). Devices are named by name or by part of their description ("HDMI", "headphones"); volume goes up to 150%. The monitors of outputs, which look like microphones, are left out.
- **Screens:** monitors are read and set with `wlr-randr` on wlroots compositors (sway, Hyprland, river, cage), `kscreen-doctor` on KDE Plasma and `xrandr` on X11. GNOME on Wayland keeps them to its own settings, so there only brightness works. A mode must be one the monitor lists, and the last screen that is on is never turned off. Brightness is the built-in panel's backlight, set through `brightnessctl` or logind (never below 5%); external monitors keep their own buttons.
- **Power:** batteries come straight from `/sys/class/power_supply`: charge, time left from the current draw, and health (what the battery holds now against what it held new). Profiles need power-profiles-daemon or tuned-ppd. Lock, suspend, reboot and power-off go through logind, so they are allowed exactly when the person at the machine could do them.
- **Bluetooth** goes through `bluetoothctl`. Pairing uses a no-input agent, which suits headphones, speakers and mice; a keyboard that shows a code to type isn't supported yet.
- **Apps** come from their desktop entries, as menus list them, your own before the system's (Flatpak and Snap exports included). An app starts from its own `Exec=` line, split into arguments as the desktop entry spec says, with each file or link one argument: no shell sees them. It runs in a systemd unit of its own (`app-orion-….service`), so it outlives Orion and logs to the journal. Files must be inside the readable roots, links must be http, https or mailto, and executable files are never opened, so opening can't run something the AI wrote.
- **The clock and language:** `timedatectl` and `localectl`, tried as you first, then as root through the [privileged helper](#the-privileged-helper) or sudo. Time zones must exist in `/usr/share/zoneinfo`, locales must be generated on the machine, and keyboard layouts must be XKB's. They are the system's defaults; GNOME and KDE keep their own per person.
- **`[desktop] approved`** lists what needs no approval, by kind: `volume`, `brightness` and `lock` by default; also `displays`, `power_profile`, `suspend`, `hibernate`, `reboot`, `poweroff`, `bluetooth` (connecting known devices), `clock` and `open` (files and links in their default app). `[desktop] apps` pre-approves apps for `app_open` by id, with wildcards: `["firefox", "org.gnome.*"]`.

`tests/desktop/run.sh` checks the sound, screen and app tools against a real headless desktop: PipeWire with dummy devices, sway on its headless backend, Xvfb, and a made-up app.

### Files

`fs_list`, `fs_read` and `fs_find` look inside the readable roots; `fs_write`, `fs_mkdir`, `fs_copy`, `fs_move` and `fs_delete` change things only inside the writable roots.

- **Symlinks are never followed out.** Copying, moving and deleting handle a link as the link itself, so a link inside a root can't reach anything outside it. Copies keep links as links.
- **Nothing is replaced unless asked** (`overwrite`, and never a folder). Copying or moving into an existing folder puts it inside, as `cp` and `mv` do. A move across filesystems copies, then removes the original.
- **Deleting goes to the Trash,** the freedesktop one file managers show and restore from. `permanent: true` deletes for good, and a folder with things in it needs `recursive: true`.
- **`fs_find`** matches names ignoring case, with `*` and `?` or any part of the name, and can look for text inside small files (up to 1 MiB), giving the line it found. It skips hidden files unless asked, and stops at 100,000 entries or 15 seconds.

### Processes

`proc_list` shows processes; `proc_signal` sends one a signal (`term` by default, or `kill`, `int`, `hup`, `stop`, `cont`, `usr1`, `usr2`), by pid or by name, then waits for it to exit. With `escalate`, what ignores `term` is killed when the wait is over.

- **Never stopped, whatever you approve:** PID 1, kernel threads, Orion itself and everything it runs under, processes of protected services, and the names in `[proc] protected` (systemd, sshd, D-Bus, NetworkManager, the display server and login manager, …).
- **A name must be unambiguous,** or `all: true` acts on every process of that name.
- **Processes a service runs are named with their service,** since systemd may start them again; `svc_control` stops a service for good.

### The journal

`journal_read` reads systemd's journal: by unit, by time (`since` and `until`, like `"1h ago"`, `"today"` or `"2026-10-20 09:00"`), by priority (`"err"` and worse), by boot (`-1` is the one before), kernel messages only, or entries containing some text. Without the `systemd-journal` group, only your own processes' entries are shown, and the result says so.

### Timers

`timer_list` lists every systemd timer, the system's and yours, with its schedule, what it runs, and when it last ran and runs next. `timer_create` schedules a tool call (`"daily"`, `"Mon..Fri 08:30"`, any systemd calendar schedule), and `timer_remove` removes one.

- **A scheduled call is an ordinary call.** The timer runs `orion call <tool> <args>`, so when it fires the call goes through the policy, the sandbox and the audit log (as `"caller": "timer <name>"`), with nobody there to approve it.
- **So only calls that would run unattended can be scheduled.** `timer_create` refuses a call the policy would only ask about, and says what to pre-approve: `[proc] allowed_commands` for `proc_run`, `[packages] updates` for `update_apply`, or the tool itself.
- **Schedules are checked by systemd** (`systemd-analyze calendar`) before anyone is asked, and the result shows the next runs.
- **They are your user timers,** in `~/.config/systemd/user/orion-timer-<name>.*`. They run while you are logged in, or always with `loginctl enable-linger`. Root work in them (security updates, say) goes through the [privileged helper](#the-privileged-helper).

### Services

`svc_list`, `svc_status` and `svc_control` work on systemd units: the machine's (`scope: "system"`, the default) or your own (`"user"`).

- **`svc_status` explains failures:** state, the last result (`exit-code`, `oom-kill`, …), restarts, memory, CPU time and the last log lines. When `svc_control` fails, the log comes with the error.
- **Protected units can't be touched,** whatever you approve: ssh, networking, the display manager, `systemd-*`, D-Bus, polkit, logins, every `.target` (so no `reboot.target`) and Orion itself. Edit `[services] protected` to change the list. Only services, sockets, timers and paths can be controlled at all.
- **`[services] allowed`** lists units the AI may control without asking (wildcards work), like `["ollama.service", "llama-server.service"]`.
- **Root:** as a normal user, system units are tried directly first, which polkit may allow. If polkit refuses, Orion retries with `sudo -n`; see `[admin] elevate`.

### Packages

`pkg_list`, `pkg_search`, `pkg_info`, `pkg_install` and `pkg_remove` drive apt, dnf, zypper or pacman (detected automatically, or set `[packages] manager`).

| Package manager | Distributions | Tested |
|---|---|---|
| apt | Debian, Ubuntu, Mint, Pop!_OS | Ubuntu 24.04, against its real repositories; Debian, against a local test repository |
| dnf (dnf4 and dnf5) | Fedora, RHEL 8+, Rocky, Alma | Fedora 44 (dnf5), against a local test repository |
| zypper | openSUSE Tumbleweed and Leap, SLES | Tumbleweed, against a local test repository |
| pacman | Arch, Manjaro, EndeavourOS | Arch, against a local test repository |

`tests/distros/run.sh` repeats the Fedora, openSUSE, Arch and Debian runs in Docker, rollbacks and unattended updates included; see the script for what it needs.

- **Nothing runs on a bad request.** Names that could be read as options are refused. Every change is dry-run with the package manager itself before you are asked (`apt-get -s`, `dnf --assumeno`, `zypper --dry-run`, `pacman -p`), so unknown packages fail first, and the result lists every package that changed, dependencies included.
- **Removal is guarded.** A removal is refused, before you are asked, if it would take away a package the system marks essential or required, or one in `[packages] protected` (the kernel, systemd, sudo, ssh, libc, the package manager, bubblewrap, …). That holds even when the package would only go as a dependent of what you named, or be replaced by one you install.
- **`[packages] allowed`** pre-approves installs by name (wildcards work), e.g. `["python3-*"]`.
- **Installs run as root and outside the sandbox,** because package scripts need that. That is why both tools are `execute`. As a normal user Orion uses `sudo -n`, which needs a `NOPASSWD` rule for the package manager, or set `[admin] elevate = "pkexec"` for a graphical password prompt ([sudoers examples](docs/install.md#6-root-access-for-the-machine-tools)).

### System updates

- **`update_check`** refreshes the package lists, then lists every package that can be upgraded, from and to which version, security fixes first. Security fixes come from the repositories' own labels: apt's `-security` suites, and the security advisories of dnf and zypper. Arch publishes none, so there nothing is marked. On Arch the check reads a private copy of the package lists, so it never leaves the system half-updated, as `pacman -Sy` alone would.
- **`update_apply`** installs updates: everything, only the security fixes (`security_only`), or only the packages named.
  - **Nothing is removed by default.** Updates can remove packages: a package replaced by a renamed one (dnf's obsoletes, pacman's replaces), or one in the way of a new dependency. On apt such upgrades are kept back, and the result says which ones were. On dnf, zypper and pacman the update is refused, naming what it would remove. `full: true` allows those removals (`dist-upgrade` on apt).
  - **Removals stay guarded.** The update is dry-run before you are asked. It is refused, even with `full`, if it would remove a package the system needs or one in `[packages] protected`.
  - **Configuration files you changed are kept** (`--force-confold`), and nothing waits on a prompt.
  - **Arch:** partial upgrades break Arch, so there it is everything or nothing.
  - **openSUSE:** Tumbleweed is upgraded with `zypper dist-upgrade`, as it must be, and Leap with `zypper update`. Security-only updates use `zypper patch --category security`.
- **After an update, both tools say what still runs old code.** Orion reads every process's memory maps for system files that were replaced, then sorts them:
  - processes that belong to a systemd service: restart that service (`svc_control`);
  - other processes: list them;
  - the kernel, `/run/reboot-required`, or PID 1: say the machine needs a reboot, and why.
- **Pre-approving updates:** set `[packages] updates = "security"` (security fixes need no approval), `"all"` (any update that removes nothing) or `"full"` (any update, also ones that remove or replace packages, as Tumbleweed's often do; protected packages still never go). With `update_apply` on `allow`, where nobody is asked, this setting is a hard limit: anything outside it is refused.
- **`orion call` exits 1** when the package manager fails partway, and the audit log records it as a failure, so scripts and timers notice.

### Snapshots and rollback

Every package change Orion makes (`pkg_install`, `pkg_remove`, `update_apply`) can be put back.

- **A history.** Before and after each change, Orion notes the version of every installed package, so the record holds exactly what moved, dependencies and new kernels included, in `~/.local/state/orion/packages.jsonl`. `pkg_history` lists the changes, newest first, with what was put back since.
- **Snapshots.** Where the root filesystem has a snapper configuration (openSUSE's default, and Fedora or Ubuntu on btrfs once `snapper -c root create-config /` is done), Orion takes a snapshot pair around each change, described "orion update_apply" and so on, and cleaned up by snapper's number algorithm. `[packages] snapshots = "required"` refuses changes when no snapshot can be taken; `"never"` turns them off.
- **`pkg_rollback`** puts a change back, by its id:
  - with a snapshot pair, `snapper undochange` reverts every file the change touched (method `snapshot`, the default when there is one). If that configuration doesn't hold the package database, the rollback says so and fails, so the other method is still open;
  - otherwise it asks the package manager for the old versions again, from the repositories (apt `name=version` with `--allow-downgrades`, dnf `name-version`, zypper `--oldpackage`) or from pacman's cache, and removes what the change added (method `packages`). Old versions first, then the removals; or the other way round when the old version can't go in beside its replacement. The removals are dry-run first and may take nothing else with them.
- **Newest first.** A change can't be put back while a later one that touched the same packages is still in place; the refusal names it. Protected packages stay installed, and a change is put back only once.
- `pkg_rollback` is `execute` and asks, like the changes themselves. Through the privileged helper, `snapper` runs only as Orion builds it, on the helper's own snapper configuration.

### Unattended updates

`[packages] automatic = "security"` (or `"all"`) has the daemon install updates once a day, at or after `automatic_at` (default `04:00`, local time). It catches up after the machine was off, and waits while on battery unless `automatic_on_battery` is set. Each run refreshes the lists and calls `update_apply` under the policy like any other caller: so `[packages] updates` must allow it (`"security"` for security fixes, `"all"` for everything), and it never removes packages. Every run is snapshotted and in the history, so `pkg_rollback` undoes it. The daemon notifies the desktop when it installed something; a failed run shows under what needs attention and tries again the next day. `orion status` shows the setting and the last run. Orion never reboots: the reboot check says when one is due.

### Network

`net_info` reports each interface (kind, state, MAC, addresses, traffic), the default IPv4 and IPv6 routes, the DNS servers (including systemd-resolved's real upstream servers), and every listening TCP and UDP port with the process behind it. As a normal user, owners show only for your own processes.

### GPUs

`gpu_info` recognizes every GPU in the machine, not just NVIDIA's. It reads the kernel's view of each display and compute device, plus `nvidia-smi` when it is there. The 4090 rendering your desktop, the Radeon beside it and the Intel graphics on the CPU all show up:

| | NVIDIA | AMD | Intel |
|---|---|---|---|
| Name | from `nvidia-smi` | from the driver, else the PCI ID database | the PCI ID database ("Arc A770", "UHD Graphics 770") |
| Memory, load, temperature, power | `nvidia-smi` | amdgpu's sysfs files and hwmon | integrated: shares system memory |
| Identity | GPU UUID | `unique_id`, as ROCm writes it | PCI address |
| Compute | CUDA | ROCm (with `/dev/kfd` and ROCm installed) | oneAPI Level Zero |

- **Each GPU gets a number by PCI address,** so it stays put across boots and drivers. CUDA's own numbering puts the fastest card first, which is why pins use UUIDs.
- **Vulkan** shows where its driver is installed. Each GPU also lists its driver and render node, and whether it drives the boot display.
- **What runs on each GPU, per process:** from `nvidia-smi` for NVIDIA, and from the kernel's DRM fdinfo for amdgpu, i915 and xe. Inference runners are traced to the model they serve: Ollama's runners through Ollama's own manifests ("qwen3:32b"), llama.cpp's server by its model file, vLLM by its model name.

**Placement: which GPU a model goes on.**

- **`model_load` says where a model will land and where it did.** It uses Ollama's scheduler's rule: the GPU with the most free memory that holds the model whole, else split across GPUs, else partly on the CPU. For example "expected: whole on GPU 2 (Radeon RX 7900 XTX)", then "placed_on: GPU 2". `model_status` and the machine panel show each loaded model's GPUs.
- **The budget is per GPU** (each card's memory, less `vram_reserve_mb`, less what is in use), and it counts only the GPUs the server may use.
- **To choose a GPU, pin the server:** `gpu_assign {"unit": "ollama", "gpus": [1]}`. Ollama decides placement itself, so the way to put models on a card is to give the server only that card. This writes a systemd drop-in (`50-orion-gpu.conf`) and restarts the service:
  - NVIDIA cards are pinned by UUID through `CUDA_VISIBLE_DEVICES`; NVIDIA cards not chosen are hidden.
  - AMD cards through `ROCR_VISIBLE_DEVICES`; Intel through `ONEAPI_DEVICE_SELECTOR`.

  An empty list removes the pin. Protected units and `services.allowed` apply as for `svc_control`. Typical uses: keep the big model on the big card, or run two servers (Ollama and a llama.cpp server) with a GPU each.
- Orion reads the pin back from the server's environment, so budgets and predictions follow it, whoever set it.

### Local models and VRAM

`model_status` finds the inference servers on this machine: `chat.local.base_url`, anything in `[models] servers`, and whatever answers on the usual ports (Ollama 11434, llama.cpp 8080, vLLM 8000, LM Studio 1234). For each it shows the kind, the process serving it, the models it offers (with their size and estimated VRAM need), and on Ollama what is loaded, how much of it is on the GPU, and when it expires. Next to that is the GPU ledger: each card's memory, the processes using it, and what is free for models.

- **The VRAM budget** is each GPU's memory less `[models] vram_reserve_mb` (1 GiB by default, for the desktop and other programs), less what is already in use, over the GPUs the server may use (see [GPUs](#gpus)). `model_load` checks a model fits before you are asked: about 1.15 × its size plus 512 MiB, for the KV cache and buffers.
- **If it doesn't fit,** the load is refused with what is in the way. With `evict: true` Orion first unloads the models due to expire soonest (the least recently used), only as many as needed.
- **`keep_alive`** sets how long a model stays loaded after its last use (`[models] keep_alive`, default `30m`; `-1` keeps it loaded).
- **Loading and pulling work on Ollama.** llama.cpp, vLLM and LM Studio serve one fixed model each; manage those as services with `svc_control`.
- The machine panel in the full-screen chat and the shell lists the loaded models with their VRAM, and any failed services.
- Without memory readings (an Intel GPU, or no `nvidia-smi`), loads aren't budgeted and `model_status` says so.

### Network changes

Orion changes the network through NetworkManager (`nmcli`), and the firewall through ufw or firewalld, whichever is installed.

- **Wi-Fi:** `wifi_scan` lists networks in range (one entry per name: the access point in use, or else the strongest). `wifi_connect` joins a saved network by name, an open one, or a new one with its password, which is saved for next time.
- **The password stays out of sight:**
  - The audit log, approval prompts and tool displays show `••••••`, and the result doesn't contain it.
  - It reaches `nmcli` through a file only the owner can read, deleted right after, never on a command line other processes could see.
  - "password" is one of the router's private words, so a message containing it goes to the local model.
- **Connections:** `net_connections` lists devices and saved connections, and `net_connection` brings one up or down, forgets it, or turns auto-connect on or off. As a normal user, NetworkManager's own permissions (polkit) usually allow this; otherwise Orion uses `[admin] elevate`.
- **Firewall:** `fw_status`, `fw_rule` (allow or deny a port or range, TCP or UDP, optionally only from an address or network) and `fw_enable`.
  - Rules Orion adds to ufw carry the comment "added by Orion".
  - While ufw is off, `fw_status` lists the rules that will apply once it's on.
- **Remote access can't be cut off by mistake.** Before anyone is asked, Orion refuses to:
  - take down or forget a connection an SSH session into this machine runs over (it reads established connections to sshd's ports and the interface they arrive on);
  - switch Wi-Fi networks when the Wi-Fi carries such a session;
  - deny the SSH port, or remove the rule that allows it;
  - turn the firewall on unless SSH is allowed from anywhere.

### Disks and mounts

- **`disk_list`:** every disk and partition with its model, size, how it's attached (NVMe, SATA, USB), filesystem and label, where it is mounted, and how full it is. "Free" means what can still be written, after quotas and reserved space. Removable drives and the disks the system runs from are marked. Where udev hasn't identified a filesystem (containers, a just-attached drive), Orion asks `blkid`.
- **`disk_usage`:** what is filling a folder: its biggest entries, staying on that filesystem, within the readable roots.
- **`disk_mount`** mounts a drive by path, `LABEL=` or `UUID=`.
  - **Where:** udisks puts it where a desktop would; otherwise `mount` puts it under `/mnt`, `/media` or a writable root (`[disks] mount_roots`), only into an empty folder.
  - **Options:** always `nosuid,nodev`. FAT, exFAT and NTFS drives are mounted as the user, so they can write.
  - **Refused:** encrypted volumes (unlock first), swap, RAID or LVM members, drives already mounted, and anything on the system's own disk.
- **`disk_unmount`** unmounts by mount point or device. `eject: true` then powers the drive off, so it's safe to unplug. If the drive is busy, it names the programs using it.
- **The system's own mounts are protected:** `/`, `/boot`, `/usr`, `/var`, `/home`, `/etc`, `/opt`, `/tmp`, `/run`, snaps and the like (`[disks] protected`). No approval can unmount them.
- **Not offered, deliberately:** formatting, partitioning, and editing `/etc/fstab`.

## System service

`orion daemon` runs Orion as a long-lived service, so every agent on the machine shares one policy, one audit log and one sandbox:

```sh
orion daemon &      # or as a systemd user service, below
orion status
# Orion daemon: running, pid 4121, up 2h 13m
# socket:       /run/user/1000/orion/orion.sock
# policy:       /home/you/.config/orion/orion.toml (1 reload)
# tools:        5 allowed, 2 ask, 0 denied
# sandbox:      bubblewrap, read-only outside the writable roots, private paths hidden, no network, no GPU
# agents:       2 connected, 9 since start
# tool calls:   41
# audit log:    /home/you/.local/state/orion/audit.jsonl
```

- **`orion serve` becomes a thin bridge.** When the daemon is running, `orion serve` connects to it and relays the MCP session, so nothing changes for clients: they still launch `orion serve`. With no daemon, or with `--direct` or an explicit `--config`, it serves in-process as before.
- **It speaks MCP over a Unix socket** at `$XDG_RUNTIME_DIR/orion/orion.sock` (`[daemon] socket` changes it). The socket is `0600` in a `0700` directory, Orion refuses to start if that directory belongs to someone else or is open to others, and every connection's peer is checked with `SO_PEERCRED` (your uid, or root).
- **Policy edits apply live.** The daemon watches the policy file and reloads it within two seconds; connected agents see the new tool list on their next call. A broken edit is reported in `orion status`, and the old policy stays in force.
- **The audit log names the caller.** Each entry carries the client's name and pid (`"caller": "claude-code (pid 5312)"`), so you can tell agents apart.
- **`orion status`** adds an `orion/status` method that any client can call; it exits 1 when the daemon isn't running.
- **The chat, the shell and `orion listen` use it too.** When the daemon runs this version of Orion with the same policy file, their tool calls go through it, so everything shares one sandbox and audit log. Their approval prompts still appear in the chat: the daemon asks the chat that made the call. Ctrl-C stops a program the daemon is running for the chat. The header says `tools run through the daemon (pid …)`; otherwise they call tools in-process, as before. `[daemon] chat = "never"` keeps them in-process.

### Approvals for AI agents

Through the daemon, agents are offered the tools the policy sets to `ask` too, and a person approves each call:

```sh
orion approvals              # prompts in this terminal, one request at a time
orion approvals --notify     # desktop notifications with Approve and Decline
orion approvals --list       # what waits now
```

```
claude-code (pid 5312) asks to run fs_write (write):
    {
      "content": "hi",
      "path": "~/orion-workspace/b.txt"
    }
  Write text to a file inside a writable root, …
Approve fs_write? [y/N] y
  approved
```

- **Where to answer:** `orion approvals` in a terminal, the notifications (`contrib/orion-approvals.service` keeps them running), or the [graphical shell](#graphical-shell), which shows each request as a card naming the agent. The first answer counts, and the others see it was answered.
- **Nobody watching:** the call is refused at once, and the agent is told someone can run `orion approvals`. **Nobody answering** within `[daemon] approval_timeout_secs` (2 minutes) counts as no.
- **What the person sees** is the agent's name and process, the tool, its description and the full arguments, with secrets blanked. Every answer is in the audit log.
- **`[daemon] approvals = false`** goes back to offering agents only the `allow` tools.

### As a systemd service

`contrib/orion.socket` and `contrib/orion.service` set up a socket-activated user service: systemd holds the socket and starts the daemon on the first connection.

```sh
cp contrib/orion.socket contrib/orion.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now orion.socket
```

The service reads `ANTHROPIC_API_KEY` and other variables from `~/.config/orion/env`, caps everything Orion runs at 8 GiB of memory and 1024 tasks through its cgroup, and hardens the daemon itself (`NoNewPrivileges`, `ProtectKernel*`, `RestrictSUIDSGID`, …). `journalctl --user -u orion` shows its log.

### The privileged helper

`orion helper` does the machine tools' root work, so Orion needs no sudo rule. It is a root service on a socket only root and the `orion` group can open (`contrib/orion-helper.socket`). It runs a command only if it is one Orion's tools build, checked again on its side against its own policy, `/etc/orion/helper.toml`, which only root may change:

- package manager commands on plain package names, whose dry run (as root) removes no protected or essential package;
- `systemctl` actions on units that aren't protected, including those that give a console root shell;
- GPU drop-ins holding only GPU variables;
- `nosuid,nodev` mounts under the mount roots, of disks the system doesn't run from;
- NetworkManager's Wi-Fi and connection commands, and firewall rules that keep SSH open.

Anything else is refused. Commands run with a clean environment and a fixed `PATH`, are logged with who asked (`journalctl -u orion-helper`), and stop if the asker hangs up. With `[admin] elevate = "auto"`, Orion uses the helper whenever its socket exists, and `orion tools` and `orion status` say how Orion gets root. [Installing Orion](docs/install.md#the-privileged-helper-recommended) sets it up; `tests/helper/run.sh` checks it end to end.

## Sandbox

Every program the AI starts with `proc_run` runs in a sandbox, whatever the policy allows it to run. `orion tools` and `orion status` show which one, and the model is told, so it reports a refusal instead of trying to get around it. (The machine tools' own fixed commands, such as `systemctl` and the package manager, run outside it. Those tools take checked arguments, not programs, and have their own rules and protected lists.)

| | bubblewrap | Landlock | limits only |
|---|---|---|---|
| Filesystem | read-only, except the `write_roots` and a private `/tmp` | read-only, except the `write_roots`, `/tmp`, `/var/tmp`, `/dev/shm` | as the user |
| Private paths (`~/.ssh`, `~/.gnupg`, …) | hidden: they don't exist | unreadable | readable |
| Network | none (own network namespace) | no TCP connect or bind (kernel ABI 4+) | open |
| GPU | no devices | no devices | open |
| Other | own PID, IPC and hostname namespaces, all capabilities dropped | can't signal or reach abstract sockets outside the sandbox (ABI 6+) | |
| Memory, CPU time, file size, no core dumps, no privilege gain | ✓ | ✓ | ✓ |
| System calls that reach past the sandbox refused (seccomp) | ✓ | ✓ | ✓ |
| A cgroup per program: memory with page cache, at most 512 processes | ✓ | ✓ | ✓ |

- **`mode = "auto"`** uses bubblewrap if it is installed and works (`apt install bubblewrap`), else Landlock (Linux 5.13+, no install needed), else only the limits. `require = true` refuses to run programs rather than fall back to limits only.
- **Hidden paths** are `[sandbox] hide` (your keys, password store, cloud credentials and Orion's own config, so a program can't loosen its policy) plus `chat.router.private_paths`, and always Orion's state directory (conversations, the audit log, the shell's token, the voiceprint).
- **`network = true`** and **`gpu = true`** open those up, for example for package installs or CUDA work. Landlock can only restrict TCP, so for a full network block install bubblewrap.
- **Limits** are per program: `memory_mb` (default 8192), `cpu_secs` (300) and `max_file_mb` (4096); `0` turns one off. Programs also can't gain privileges through setuid binaries.
- **A cgroup per program:** where the systemd user manager runs, each program gets a scope of its own (`systemd-run --user --scope`), so `memory_mb` holds for all of its processes together, page cache included, and `max_processes` (512) stops a fork bomb. Without a user manager the per-process limits above still apply. `cgroup = false` turns scopes off.
- **A seccomp filter** refuses, with "operation not permitted", the system calls a program has no business making here: debugging other processes (ptrace), loading kernels, modules or eBPF, perf events, userfaultfd, io_uring, mounting, `setns` and new user namespaces, the kernel keyring, swap, reboot, setting the clock, raw I/O ports and `open_by_handle_at`. A system call from another architecture (a 32-bit call in a 64-bit program) ends the program. Built into Orion (no libseccomp), for x86_64 and aarch64. `seccomp = false` turns it off, for a program that needs `strace` or `gdb`.
- Each result says which sandbox ran it (`"sandbox": "bubblewrap"`), and `orion tools` prints the current one.
- Under Landlock, a directory that contains a hidden path is opened up entry by entry, so files created there later aren't readable. Bubblewrap has no such gap.

## Policy

Copy [`orion.example.toml`](orion.example.toml) to `~/.config/orion/orion.toml`, or point `--config` or `ORION_CONFIG` at a file.

- `[defaults]` sets `allow`, `ask` or `deny` for each risk level (`read`, `write`, `execute`).
- `[tools]` overrides individual tools. A model is only offered tools it could actually use: `orion chat` offers `allow` and `ask` tools, and `orion serve` offers only `allow` tools.
- `[fs]` sets `read_roots` and `write_roots` (default `~/orion-workspace`). These are hard limits, checked *before* you are asked, so you never approve a call that would fail. Paths are canonicalized before the check, so `..` and symlinks cannot escape a root, and writes to paths that don't exist yet are checked through their nearest real ancestor.
- `[proc]` sets `allowed_commands`: pre-approved programs, matched exactly. When `proc_run` is `ask`, these run without a prompt and anything else asks. When it is `allow`, only these run at all. `[proc]` also sets a timeout and an output cap. Once a program exits or times out, its whole process group is killed, so background children cannot linger.
- `[audit]` controls the append-only JSONL log at `~/.local/state/orion/audit.jsonl`. It records each decision and, for `ask`, your answer. Long string arguments are truncated in the log.
- `[chat]` holds the front-end settings above.
- `[services]`, `[packages]`, `[models]` and `[admin]` configure [the machine](#the-machine) tools.
- `[sandbox]` and `[daemon]` configure the [sandbox](#sandbox) and the [system service](#system-service).

Unknown keys are an error, so a misspelled setting fails loudly instead of being ignored.

## Layout

```
src/
  lib.rs        Orion: the registry that applies the policy and audit to each call
  tool.rs       the Tool trait and risk levels
  policy.rs     policy file, decisions, path scoping
  audit.rs      JSONL audit log
  mcp.rs        MCP server (initialize, ping, tools/list, tools/call)
  daemon.rs     the system service: socket, peer checks, live reload, the stdio bridge
  approvals.rs  approvals through the daemon: the broker, and `orion approvals`
  tools/session.rs  the person's desktop session (display, compositor, session bus),
                for the desktop tools: audio, display, power, bluetooth, apps, clock
  watch.rs      what needs attention: the checks, the daemon's watch, system_health,
                unattended updates
  remote.rs     the chat calling tools through the daemon
  helper.rs     the privileged helper: the root service, its client, who may use it;
                each tool module checks its own commands (helper_check)
  sandbox.rs    sandboxing for proc_run: bubblewrap, Landlock, resource limits
  tools/        built-in tools: system, fs and fileops (copy, move, delete, find),
                process (list, run, signal), network, services (systemd), journal,
                timers, packages, models (inference servers, VRAM budget), gpu
                (recognition, per-process use, placement, gpu_assign), netconf
                (Wi-Fi, connections, firewall), disks (disks, usage, mounts), updates
                (pending updates, upgrading, restarts and reboots), changes
                (snapshots, the package history, pkg_rollback);
                exec.rs runs programs for them
  chat/
    mod.rs      config, the Backend and Ui traits
    session.rs  one conversation across both models: routing, catch-up, fallback, tool loop
    memory.rs   what the AI remembers about each person
    router.rs   the routing order, keyword rules and the private-path guard
    classifier.rs  the local model as routing judge
    claude.rs   Claude backend: streaming Messages API client (SSE)
    local.rs    local backend: OpenAI-compatible chat completions (Ollama, llama.cpp, vLLM, LM Studio)
    http.rs     shared HTTP: retries, proxy rules, error bodies, SSE lines
    store.rs    saved conversations: atomic, private JSON files
    terminal.rs the line-by-line chat, shared commands, `orion conversations`
    agent.rs    the conversation's own thread, shared by the full-screen chat, the shell
                and headless voice
    listen.rs   headless voice (`orion listen`): wake, spoken replies, spoken approvals
    machine.rs  live CPU, memory and GPU readings for status displays
    tui/        the full-screen chat: screen and keys (mod.rs), the message editor (input.rs)
    shell/      the graphical shell: server, event hub and security (mod.rs),
                minimal HTTP (http.rs), the page (shell.html)
  interrupt.rs  Ctrl-C: the SIGINT flag, interruptible waits and reads
  voice/        listening and the pause detector (record.rs), speech to text
                (transcribe.rs), text to speech and sentence splitting (speak.rs),
                the wake word (wake.rs), speaker verification (speaker.rs), the
                neural speaker encoder (neural.rs), reading PyTorch weights
                (torch.rs) and enrolling a voice (enroll.rs)
docs/           install.md and operating.md: the installation and operating guides
packaging/      release files: build.sh (tarball, .deb, .rpm), pkgbuild.sh (Arch),
                install.sh (any distribution), assemble.sh, and test.sh, which
                installs them in clean containers
contrib/        orion.socket and orion.service (the daemon), orion-approvals.service
                (approvals by notification), orion-helper.socket and
                orion-helper.service (the privileged helper), orion-voice.service
                (headless voice), orion-kiosk (boot into the shell), orion-shell.service
tests/          end-to-end runs of the real binary: stdio.rs (MCP), daemon.rs
                (daemon plus bridge), distros/ (package and update tools,
                rollbacks and unattended updates on Fedora, openSUSE, Arch and
                Debian, in Docker), helper/ (the privileged helper and its
                snapshots, as root), desktop/ (sound, screens and apps on a real
                headless desktop)
.github/        ci.yml: lint, tests, the oldest supported Rust, distros/ and
                helper/, on every push and nightly; release.yml: the release
                files, built and installed in clean containers on every push,
                published as a GitHub release for a v* tag
```

To add a capability, implement `Tool` in `src/tools/` and register it in `tools::builtin()`. Pick its `Risk` honestly, because that is what the policy defaults key off. Put checks that no approval should override in `precheck`, and argument-dependent pre-approval (like `proc_run`'s command list) in `preapproval`.

## Roadmap

1. ~~**Interactive approval.**~~ Done in `orion chat`, via the `ask` decision.
2. ~~**System daemon.**~~ Done: `orion daemon`, socket-activated by systemd, with `orion serve` as the bridge. The chat, the shell and `orion listen` call tools through it, and ~~`ask` approvals from MCP agents~~ reach you in a terminal, a notification or the shell.
3. ~~**Isolation.**~~ Done: bubblewrap or Landlock with per-program limits, and ~~a privileged helper~~ (`orion helper`) for root work in place of sudo rules. ~~A cgroup per call~~ (memory including page cache, a process limit), ~~a seccomp filter~~ and ~~a tamper-evident audit log~~ are done too. Next: GPU memory budgets per program, which cgroups can't express yet.
4. ~~**Richer machine model.**~~ Done: services, packages, network, and local models within a VRAM budget. ~~GPU recognition and placement~~, ~~network changes~~ (Wi-Fi, connections, firewall) and ~~disks and mounts~~ are done too, and so are ~~system updates~~ (security fixes, restarts and reboots), on apt, dnf, zypper and pacman. ~~Unattended updates~~, with ~~snapshots and rollback~~ under every package change, are done too, and so is ~~the desktop~~: sound, screens and brightness, power, Bluetooth, apps, the clock and language. Next: a status-bar badge for pending security fixes.
5. ~~**Deeper monitoring.**~~ Done: drives' SMART health, temperatures, battery wear, bursts of errors and the kernel's hardware errors in the journal, and repeated failed logins, alongside the earlier checks.
6. ~~**Packaging.**~~ Done: release binaries for x86_64 and aarch64 (glibc 2.31 and newer), `.deb`, `.rpm`, an Arch PKGBUILD and an install script, each installed and tested in clean containers before a release is published.
7. ~~**Proactive monitoring.**~~ Done: the daemon watches for failed services, full disks, security updates, a due reboot and low memory, notifies the desktop, and tells the AI.
8. **Front-end, next steps.** ~~A terminal chat~~, ~~local models~~ and ~~automatic routing~~ (rules plus a local-model judge) are done. ~~Hotkeys~~ (Ctrl-C to interrupt, Shift-Tab, Ctrl-O), ~~saved conversations~~, ~~a full-screen TUI~~, ~~voice~~, ~~a graphical shell~~ and ~~wake-word listening~~ are done too. A ~~headless voice mode~~ (`orion listen`), ~~speaker verification~~, ~~a neural speaker model~~ and ~~several enrolled people~~ are done too. ~~Per-person conversations and memory~~ are done as well. Next for voice: a benchmark on real recordings.

## License

Copyright (c) 2026 Goliaith. All rights reserved: Orion is proprietary, and no license is granted to use, copy, modify or distribute it without written permission. See [LICENSE](LICENSE). The third-party libraries it uses keep their own licenses.
