# Orion

Orion is an operating system built around AI: Linux underneath, with a model as a first-class user of the machine.

This repository holds the first two pieces:

- **The interface between the AI and the system.** A model never touches the machine directly. It talks to `orion`, which offers the machine as a small set of typed tools. A local policy decides what is allowed, and every call is written to an audit log.
- **The front-end**, `orion chat`. You talk to an AI in your terminal, and it works on the machine through those same tools. Each message goes to Claude or to a model running on this machine's own hardware, whichever fits: private and simple work stays local, complex work goes to Claude. Anything risky stops and asks you first.

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

## Tools

| Tool          | Risk    | Default | What it does |
|---------------|---------|---------|--------------|
| `system_info` | read    | allow   | Hostname, OS, kernel, CPU, memory, load, uptime |
| `gpu_info`    | read    | allow   | GPUs by vendor; for NVIDIA, memory, utilization and temperature via `nvidia-smi` |
| `fs_list`     | read    | allow   | Directory listing inside the readable roots |
| `fs_read`     | read    | allow   | Read a file, or a byte range of one, inside the readable roots |
| `fs_write`    | write   | ask     | Write or append to a file inside the writable roots |
| `proc_list`   | read    | allow   | Processes, with filtering and sorting by memory |
| `proc_run`    | execute | ask     | Run one program (no shell), with a timeout and an output cap |

Out of the box the AI can look at the machine freely. Changing it (writing files, running programs) needs your approval each time. With no human present (`orion serve`), "ask" means no.

## Build

```sh
cargo build --release      # binary: target/release/orion
cargo test
```

Linux only, because the tools read `/proc` and `/sys` directly.

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
- **Commands:** `/use`, `/route`, `/model`, `/models`, `/effort`, `/tools`, `/usage`, `/voice`, `/keys`, `/new`, `/history`, `/resume`, `/title`, `/exit`, plus `@claude` / `@local` prefixes and the [hotkeys](#hotkeys). Line history is kept in `~/.local/state/orion/chat_history`.
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

### Hotkeys

| Key | What it does |
|---|---|
| **Ctrl-C** | Stops the AI mid-turn: a streaming reply, a long think, a running program or an approval prompt. Any partial reply is kept and marked as interrupted, so the model knows where it stopped. At the prompt, press it twice to quit. |
| **Shift-Tab** | Cycles routing between auto, claude and local, keeping what you've typed. Models that can't be reached are skipped. |
| **Ctrl-T** | Talks: listens until you pause, then puts what you said in the message box. See [Voice](#voice). |
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
orion conversations                           # saved chats (see "Saved conversations")
orion shell                                   # the graphical shell (see "Graphical shell")
orion serve                                   # MCP server on stdin/stdout
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

## Policy

Copy [`orion.example.toml`](orion.example.toml) to `~/.config/orion/orion.toml`, or point `--config` or `ORION_CONFIG` at a file.

- `[defaults]` sets `allow`, `ask` or `deny` for each risk level (`read`, `write`, `execute`).
- `[tools]` overrides individual tools. A model is only offered tools it could actually use: `orion chat` offers `allow` and `ask` tools, and `orion serve` offers only `allow` tools.
- `[fs]` sets `read_roots` and `write_roots` (default `~/orion-workspace`). These are hard limits, checked *before* you are asked, so you never approve a call that would fail. Paths are canonicalized before the check, so `..` and symlinks cannot escape a root, and writes to paths that don't exist yet are checked through their nearest real ancestor.
- `[proc]` sets `allowed_commands`: pre-approved programs, matched exactly. When `proc_run` is `ask`, these run without a prompt and anything else asks. When it is `allow`, only these run at all. `[proc]` also sets a timeout and an output cap. Once a program exits or times out, its whole process group is killed, so background children cannot linger.
- `[audit]` controls the append-only JSONL log at `~/.local/state/orion/audit.jsonl`. It records each decision and, for `ask`, your answer. Long string arguments are truncated in the log.
- `[chat]` holds the front-end settings above.

Unknown keys are an error, so a misspelled setting fails loudly instead of being ignored.

## Layout

```
src/
  lib.rs        Orion: the registry that applies the policy and audit to each call
  tool.rs       the Tool trait and risk levels
  policy.rs     policy file, decisions, path scoping
  audit.rs      JSONL audit log
  mcp.rs        MCP server (initialize, ping, tools/list, tools/call)
  tools/        built-in tools: system, fs, process
  chat/
    mod.rs      config, the Backend and Ui traits
    session.rs  one conversation across both models: routing, catch-up, fallback, tool loop
    router.rs   the routing order, keyword rules and the private-path guard
    classifier.rs  the local model as routing judge
    claude.rs   Claude backend: streaming Messages API client (SSE)
    local.rs    local backend: OpenAI-compatible chat completions (Ollama, llama.cpp, vLLM, LM Studio)
    http.rs     shared HTTP: retries, proxy rules, error bodies, SSE lines
    store.rs    saved conversations: atomic, private JSON files
    terminal.rs the line-by-line chat, shared commands, `orion conversations`
    agent.rs    the conversation's own thread, shared by the full-screen chat and the shell
    machine.rs  live CPU, memory and GPU readings for status displays
    tui/        the full-screen chat: screen and keys (mod.rs), the message editor (input.rs)
    shell/      the graphical shell: server, event hub and security (mod.rs),
                minimal HTTP (http.rs), the page (shell.html)
contrib/        orion-kiosk (boot into the shell), orion-shell.service
  interrupt.rs  Ctrl-C: the SIGINT flag, interruptible waits and reads
  voice/        listening and the pause detector (record.rs), speech to text
                (transcribe.rs), text to speech and sentence splitting (speak.rs)
tests/stdio.rs  end-to-end MCP session against the real binary
```

To add a capability, implement `Tool` in `src/tools/` and register it in `tools::builtin()`. Pick its `Risk` honestly, because that is what the policy defaults key off. Put checks that no approval should override in `precheck`, and argument-dependent pre-approval (like `proc_run`'s command list) in `preapproval`.

## Roadmap

1. ~~**Interactive approval.**~~ Done in `orion chat`, via the `ask` decision.
2. **System daemon.** Run `oriond` as a systemd service on a Unix socket, with `orion serve` as a thin stdio bridge, so many agents share one policy and one log.
3. **Isolation.** Run `proc_run` inside namespaces, cgroups and seccomp (or Landlock), with CPU, memory and GPU limits per call.
4. **Richer machine model.** Add services (systemd), packages, network, GPU scheduling and model or runtime management (local inference servers, VRAM budgeting).
5. **Front-end, next steps.** ~~A terminal chat~~, ~~local models~~ and ~~automatic routing~~ (rules plus a local-model judge) are done. ~~Hotkeys~~ (Ctrl-C to interrupt, Shift-Tab, Ctrl-O), ~~saved conversations~~, ~~a full-screen TUI~~, ~~voice~~ and ~~a graphical shell~~ are done too. Still to come: wake-word listening.
