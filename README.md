# Orion

Orion is an operating system built around AI: Linux underneath, with a model as a first-class user of the machine.

This repository holds the first two pieces:

- **The interface between the AI and the system.** A model never touches the machine directly. It talks to `orion`, which offers the machine as a small set of typed tools. A local policy decides what is allowed, and every call is written to an audit log.
- **The front-end**, `orion chat`. You talk to an AI in your terminal, either Claude or a model running on this machine's own hardware, and it works on the machine through those same tools. Anything risky stops and asks you first.

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
claude-opus-5-5 · effort high · 7 tools (5 automatic, 2 ask first) · /help for commands

orion › what's eating my memory? write a short report

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
- **Commands:** `/tools`, `/model`, `/models`, `/use claude|local`, `/effort`, `/usage`, `/clear`, `/exit`. Line history is kept in `~/.local/state/orion/chat_history`.
- Settings live under `[chat]` in the policy file: the default provider, and per-provider sections `[chat.claude]` (model, default `claude-opus-5-5`; effort; progress notes; fallbacks) and `[chat.local]`.

`ANTHROPIC_AUTH_TOKEN` (an OAuth access token) also works in place of an API key.

### Local models

Orion can use a model running on your own GPU instead of Claude. Nothing leaves the machine, and no API key is needed. It speaks the OpenAI-compatible chat API (`/v1/chat/completions`, with streaming and tool calls), which every major local runtime serves:

| Runtime | Start it | `[chat.local] base_url` |
|---|---|---|
| [Ollama](https://ollama.com) | `ollama pull qwen3:8b` (the server runs as a service) | `http://127.0.0.1:11434/v1` (default) |
| llama.cpp | `llama-server -m model.gguf --jinja` | `http://127.0.0.1:8080/v1` |
| vLLM | `vllm serve Qwen/Qwen3-8B --enable-auto-tool-choice --tool-call-parser hermes` | `http://127.0.0.1:8000/v1` |
| LM Studio | start the local server in the app | `http://127.0.0.1:1234/v1` |

```sh
orion chat --local                    # first model the server lists
orion chat --local --model qwen3:8b   # a specific one
```

Or set `provider = "local"` under `[chat]` to make it the default. Inside a chat, `/models` lists what the server has, `/model <id>` switches, and `/use claude` or `/use local` swaps provider, which starts a new conversation.

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
    mod.rs      the Backend trait and the tool loop: rounds, approvals, results
    claude.rs   Claude backend: streaming Messages API client (SSE)
    local.rs    local backend: OpenAI-compatible chat completions (Ollama, llama.cpp, vLLM, LM Studio)
    http.rs     shared HTTP: retries, proxy rules, error bodies, SSE lines
    terminal.rs the interactive terminal UI
tests/stdio.rs  end-to-end MCP session against the real binary
```

To add a capability, implement `Tool` in `src/tools/` and register it in `tools::builtin()`. Pick its `Risk` honestly, because that is what the policy defaults key off. Put checks that no approval should override in `precheck`, and argument-dependent pre-approval (like `proc_run`'s command list) in `preapproval`.

## Roadmap

1. ~~**Interactive approval.**~~ Done in `orion chat`, via the `ask` decision.
2. **System daemon.** Run `oriond` as a systemd service on a Unix socket, with `orion serve` as a thin stdio bridge, so many agents share one policy and one log.
3. **Isolation.** Run `proc_run` inside namespaces, cgroups and seccomp (or Landlock), with CPU, memory and GPU limits per call.
4. **Richer machine model.** Add services (systemd), packages, network, GPU scheduling and model or runtime management (local inference servers, VRAM budgeting).
5. **Front-end, next steps.** ~~A terminal chat~~ and ~~local models~~ are done. Still to come: interrupting a reply with Ctrl-C, saving and resuming conversations, a full-screen TUI, voice, a graphical shell, and routing between local and hosted models automatically (for example, local for private or simple tasks).
