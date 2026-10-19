# Orion

Orion is an operating system built around AI: Linux underneath, with a model as a first-class user of the machine.

This repository holds the first piece, **the interface between the AI and the system**. A model never touches the machine directly. It talks to `orion`, which offers the machine as a small set of typed tools. A local policy decides what is allowed, and every call is written to an audit log.

```
  ┌──────────────┐  MCP (JSON-RPC over stdio)  ┌────────────────────────────┐
  │  AI model /  │ ──────────────────────────▶ │ orion                      │
  │  agent host  │ ◀────────────────────────── │  policy → tool → audit log │
  └──────────────┘                             └─────────────┬──────────────┘
   Claude, a local                                           │
   model, any MCP client                         /proc, /sys, files, programs
```

It speaks the [Model Context Protocol](https://modelcontextprotocol.io), so the same interface works for hosted models, local models and future in-house agents. Nothing in it is tied to one vendor.

## Tools

| Tool          | Risk    | Default | What it does |
|---------------|---------|---------|--------------|
| `system_info` | read    | allow   | Hostname, OS, kernel, CPU, memory, load, uptime |
| `gpu_info`    | read    | allow   | GPUs by vendor; for NVIDIA, memory, utilization and temperature via `nvidia-smi` |
| `fs_list`     | read    | allow   | Directory listing inside the readable roots |
| `fs_read`     | read    | allow   | Read a file, or a byte range of one, inside the readable roots |
| `fs_write`    | write   | deny    | Write or append to a file inside the writable roots |
| `proc_list`   | read    | allow   | Processes, with filtering and sorting by memory |
| `proc_run`    | execute | deny    | Run one allow-listed program (no shell), with a timeout and an output cap |

Out of the box the AI can look at the machine but cannot change it. You open up writing and execution on purpose, in the policy.

## Build

```sh
cargo build --release      # binary: target/release/orion
cargo test
```

Linux only, because the tools read `/proc` and `/sys` directly.

## Use

```sh
orion tools                                   # every tool, its risk, and the policy decision
orion call system_info                        # call a tool yourself, exactly as the AI would
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

- `[defaults]` sets allow or deny for each risk level (`read`, `write`, `execute`).
- `[tools]` overrides individual tools. Denied tools are left out of `tools/list`, so the model is never offered something it cannot use.
- `[fs]` sets `read_roots` and `write_roots`. Paths are canonicalized before the check, so `..` and symlinks cannot escape a root, and writes to paths that don't exist yet are checked through their nearest real ancestor.
- `[proc]` sets an exact-match `allowed_commands` list, a timeout and an output cap. Once a program exits or times out, its whole process group is killed, so background children cannot linger.
- `[audit]` controls the append-only JSONL log at `~/.local/state/orion/audit.jsonl`. Long string arguments are truncated in the log.

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
tests/stdio.rs  end-to-end MCP session against the real binary
```

To add a capability, implement `Tool` in `src/tools/` and register it in `tools::builtin()`. Pick its `Risk` honestly, because that is what the policy defaults key off.

## Roadmap

1. **Interactive approval.** Add an `ask` decision that routes risky calls to the human, through a desktop prompt or `orion approve`, instead of a static allow or deny.
2. **System daemon.** Run `oriond` as a systemd service on a Unix socket, with `orion serve` as a thin stdio bridge, so many agents share one policy and one log.
3. **Isolation.** Run `proc_run` inside namespaces, cgroups and seccomp (or Landlock), with CPU, memory and GPU limits per call.
4. **Richer machine model.** Add services (systemd), packages, network, GPU scheduling and model or runtime management (local inference servers, VRAM budgeting).
5. **Front-end.** Build the human-facing shell where you talk to the AI that runs the machine.
