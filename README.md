# Orion

Orion is an operating system built around AI: Linux underneath, with a model as a first-class user of the machine.

This repository holds the first two pieces:

- **The interface between the AI and the system.** A model never touches the machine directly. It talks to `orion`, which offers the machine as a small set of typed tools. A local policy decides what is allowed, and every call is written to an audit log.
- **The front-end**, `orion chat`. You talk to Claude in your terminal, and Claude works on the machine through those same tools. Anything risky stops and asks you first.

```
   you ──▶ orion chat ──(Claude API)──▶ Claude
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
claude-opus-5-5 · effort high · 7 tools (5 automatic, 2 ask first) · /help for commands

orion › what's eating my memory? write a short report

I'll check the hardware and the biggest processes.
  ⚙ system_info
    ✓ 0ms
  ⚙ proc_list {"limit":5,"sort":"memory"}
    ✓ 3ms
  ⚙ fs_write {"content":"…","path":"~/orion-workspace/memory.txt"}
  ? Claude wants to run fs_write:
    { "content": "…", "path": "~/orion-workspace/memory.txt" }
  Allow? [y]es / [n]o / [a]lways for this session › y
    ✓ 1ms
Firefox is using 3.1 GB of your 16 GB. I wrote the details to ~/orion-workspace/memory.txt.
```

- **Streaming.** Replies stream as they are written. Claude's short progress notes between tool calls show dimmed, so long tasks don't sit silent.
- **Every tool call is visible**, with its arguments and whether it worked.
- **Approval prompts** show the full arguments of anything that changes the machine. `a` approves that tool for the rest of the session.
- **Commands:** `/tools`, `/model`, `/effort`, `/usage`, `/clear`, `/exit`. Line history is kept in `~/.local/state/orion/chat_history`.
- Settings live under `[chat]` in the policy file: model (default `claude-opus-5-5`), effort, display of progress notes, and server-side fallbacks.

`ANTHROPIC_AUTH_TOKEN` (an OAuth access token) also works in place of an API key.

## Other commands

```sh
orion tools                                   # every tool, its risk, and the policy decision
orion call system_info                        # call a tool yourself, exactly as the AI would (prompts for "ask" tools)
orion call fs_list '{"path": "~"}'
orion audit -n 20                             # recent calls, as JSON lines
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
    claude.rs   streaming Claude Messages API client (SSE, retries)
    mod.rs      the agent loop: append-only history, tool rounds, approvals
    terminal.rs the interactive terminal UI
tests/stdio.rs  end-to-end MCP session against the real binary
```

To add a capability, implement `Tool` in `src/tools/` and register it in `tools::builtin()`. Pick its `Risk` honestly, because that is what the policy defaults key off. Put checks that no approval should override in `precheck`, and argument-dependent pre-approval (like `proc_run`'s command list) in `preapproval`.

## Roadmap

1. ~~**Interactive approval.**~~ Done in `orion chat`, via the `ask` decision.
2. **System daemon.** Run `oriond` as a systemd service on a Unix socket, with `orion serve` as a thin stdio bridge, so many agents share one policy and one log.
3. **Isolation.** Run `proc_run` inside namespaces, cgroups and seccomp (or Landlock), with CPU, memory and GPU limits per call.
4. **Richer machine model.** Add services (systemd), packages, network, GPU scheduling and model or runtime management (local inference servers, VRAM budgeting).
5. **Front-end, next steps.** ~~A terminal chat~~ is done. Still to come: interrupting a reply with Ctrl-C, saving and resuming conversations, a full-screen TUI, voice, local models (running on the GPU) alongside Claude, and a graphical shell.
