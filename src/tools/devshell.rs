//! Developer mode's tools: a persistent shell (`dev_shell`,
//! `dev_shell_read`, `dev_shell_close`) and the dev root's checkpoints
//! (`dev_history`, `dev_rollback`). See `crate::dev`.
//!
//! A shell is one bash, in the dev sandbox the mode in force says, that
//! stays open between calls: its working directory, environment, virtual
//! environments and background jobs carry over. Opening one asks the
//! person; the commands sent to it then run without asking, the sandbox
//! being the boundary. A shell ends when it is closed, when the mode
//! changes or expires, or with the process holding it (the daemon, when
//! calls go through it).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::dev::checkpoint::Store;
use crate::dev::{self, Mode};
use crate::policy::Policy;
use crate::tool::{parse_args, Preapproval, Risk, Tool};

/// Shells open at once, in this process.
const MAX_SHELLS: usize = 4;
/// Output kept per shell, for `dev_shell_read`.
const KEEP_BYTES: usize = 4 << 20;

/// What a shell printed (stdout and stderr together), as offsets from its start.
#[derive(Default)]
struct Output {
    buf: Vec<u8>,
    /// Bytes dropped from the front of `buf`.
    base: usize,
    closed: bool,
}

impl Output {
    fn end(&self) -> usize {
        self.base + self.buf.len()
    }

    fn since(&self, from: usize) -> &[u8] {
        &self.buf[from.saturating_sub(self.base).min(self.buf.len())..]
    }

    /// What is still kept from `from` up to `to`.
    fn between(&self, from: usize, to: usize) -> &[u8] {
        let start = from.max(self.base);
        &self.since(start)[..to.saturating_sub(start).min(self.end().saturating_sub(start))]
    }

    /// The end marker of the command that started at `from`: where it
    /// begins (from the shell's start), the exit code and the folder.
    fn marker(&self, from: usize, nonce: &str) -> Option<(usize, i32, String)> {
        // What `since` returns begins at `from`, or at `base` if that has gone.
        find_marker(self.since(from), nonce).map(|(at, rc, cwd)| (from.max(self.base) + at, rc, cwd))
    }
}

type Shared = Arc<(Mutex<Output>, Condvar)>;

/// The command a shell is running.
struct Running {
    nonce: String,
    from: usize,
    started: Instant,
}

struct Shell {
    mode: Mode,
    child: Child,
    stdin: ChildStdin,
    out: Shared,
    running: Option<Running>,
    /// How far `dev_shell_read` has shown.
    seen: usize,
}

impl Shell {
    fn open(policy: &Policy, mode: Mode) -> Result<Self> {
        let confined = dev::shell_policy(policy, mode)?;
        let root = policy.dev.root();
        let (mut cmd, backend, guard) =
            crate::sandbox::command(&confined, "bash", &["--noprofile".into(), "--norc".into()], Some(&root))?;
        ensure!(backend == crate::sandbox::Backend::Bubblewrap, "developer mode needs bubblewrap, and got {}", backend.name());
        cmd.env("TERM", "dumb")
            .env("ORION_DEV", if mode == Mode::Unsandboxed { "unsandboxed" } else { "sandboxed" })
            .env_remove("PROMPT_COMMAND")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("starting the dev shell")?;
        drop(guard);
        let mut stdin = child.stdin.take().context("no stdin")?;
        let out: Shared = Arc::new((Mutex::new(Output::default()), Condvar::new()));
        for pipe in [child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>), child.stderr.take().map(|p| Box::new(p) as _)]
            .into_iter()
            .flatten()
        {
            let out = out.clone();
            std::thread::spawn(move || pump(pipe, &out));
        }
        // From here on stderr comes through stdout, in order.
        stdin.write_all(b"exec 2>&1\n")?;
        Ok(Self { mode, child, stdin, out, running: None, seen: 0 })
    }

    fn start(&mut self, command: &str) -> Result<()> {
        ensure!(self.running.is_none(), "this shell is still running a command; follow it with dev_shell_read, or close the shell");
        let nonce = format!("__orion_done_{}", random_hex());
        let encoded = base64(command.as_bytes());
        // Through eval, so a command that doesn't parse can't swallow the
        // marker; from /dev/null, so it can't read what comes next.
        let script = format!(
            "eval \"$(printf %s '{encoded}' | base64 -d)\" </dev/null; printf '\\n{nonce} %d %s\\n' \"$?\" \"$PWD\"\n"
        );
        let from = self.out.0.lock().unwrap_or_else(|e| e.into_inner()).end();
        self.stdin.write_all(script.as_bytes()).context("the shell has exited")?;
        self.stdin.flush()?;
        self.running = Some(Running { nonce, from, started: Instant::now() });
        self.seen = from;
        Ok(())
    }

    /// Wait up to `wait` for the running command; what it printed since
    /// `dev_shell_read` last looked, and how it ended if it has.
    fn follow(&mut self, wait: Duration, cap: usize) -> Value {
        let (lock, cv) = &*self.out;
        let deadline = Instant::now() + wait;
        let mut out = lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut ended_with_mode = false;
        let ended = loop {
            let done = self.running.as_ref().and_then(|r| out.marker(r.from, &r.nonce));
            if done.is_some() || out.closed || self.running.is_none() {
                break done;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break None;
            }
            // A build doesn't outlast the mode it was started in.
            if dev::mode() != self.mode {
                ended_with_mode = true;
                break None;
            }
            out = cv.wait_timeout(out, left.min(Duration::from_secs(2))).unwrap_or_else(|e| e.into_inner()).0;
        };
        let (text, exit, cwd) = match &ended {
            Some((marker, rc, cwd)) => {
                let mut text = String::from_utf8_lossy(out.between(self.seen, *marker)).into_owned();
                // The newline the marker brought along.
                if text.ends_with('\n') {
                    text.pop();
                }
                (text, Some(*rc), Some(cwd.clone()))
            }
            None => (String::from_utf8_lossy(out.since(self.seen)).into_owned(), None, None),
        };
        let mut closed = out.closed;
        let end = out.end();
        drop(out);
        if ended_with_mode {
            self.kill();
            closed = true;
        }
        let duration = self.running.as_ref().map(|r| r.started.elapsed().as_millis());
        if ended.is_some() || closed {
            self.running = None;
            self.seen = end;
        } else {
            self.seen = end;
        }
        let (output, truncated) = tail(text, cap);
        json!({
            "output": output,
            "output_truncated": truncated,
            "exit_code": exit,
            "running": ended.is_none() && !closed && self.running.is_some(),
            "shell_exited": closed,
            "cwd": cwd,
            "duration_ms": duration,
        })
    }

    fn kill(&mut self) {
        // The whole group: bwrap, and with it the namespace's processes.
        // SAFETY: kill(2) has no memory-safety preconditions.
        unsafe { libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL) };
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        self.kill();
    }
}

fn pump(mut pipe: Box<dyn Read + Send>, out: &Shared) {
    let mut chunk = [0u8; 8192];
    loop {
        let n = pipe.read(&mut chunk).unwrap_or(0);
        let (lock, cv) = &**out;
        let mut o = lock.lock().unwrap_or_else(|e| e.into_inner());
        if n == 0 {
            o.closed = true;
            cv.notify_all();
            return;
        }
        o.buf.extend_from_slice(&chunk[..n]);
        if o.buf.len() > KEEP_BYTES {
            let drop = o.buf.len() - KEEP_BYTES;
            o.buf.drain(..drop);
            o.base += drop;
        }
        cv.notify_all();
    }
}

/// Where the marker line starts in `text`, the exit code and the folder.
fn find_marker(text: &[u8], nonce: &str) -> Option<(usize, i32, String)> {
    let needle = format!("\n{nonce} ");
    let at = text.windows(needle.len()).position(|w| w == needle.as_bytes())?;
    let rest = &text[at + needle.len()..];
    let eol = rest.iter().position(|&b| b == b'\n')?;
    let line = String::from_utf8_lossy(&rest[..eol]);
    let (rc, cwd) = line.split_once(' ').unwrap_or((&line, ""));
    // The newline before the marker belongs to it.
    Some((at + 1, rc.parse().unwrap_or(-1), cwd.to_string()))
}

/// The last `cap` bytes of `text`, on a character boundary.
fn tail(text: String, cap: usize) -> (String, bool) {
    if text.len() <= cap {
        return (text, false);
    }
    let mut cut = text.len() - cap;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    (text[cut..].to_string(), true)
}

fn random_hex() -> String {
    let mut bytes = [0u8; 12];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut bytes);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

type Shells = Mutex<HashMap<String, Arc<Mutex<Shell>>>>;

/// This process's shells, and the watch that ends them with the mode.
fn shells() -> &'static Shells {
    static SHELLS: OnceLock<Shells> = OnceLock::new();
    SHELLS.get_or_init(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(Duration::from_secs(5));
            end_stale();
        });
        Mutex::new(HashMap::new())
    })
}

/// Close the shells the mode in force no longer allows, and the ones whose
/// bash has gone.
fn end_stale() {
    let mode = dev::mode();
    let mut all = shells().lock().unwrap_or_else(|e| e.into_inner());
    all.retain(|_, s| {
        let Ok(mut s) = s.try_lock() else { return true };
        s.mode == mode && !matches!(s.child.try_wait(), Ok(Some(_)))
    });
}

fn get(id: &str) -> Result<Arc<Mutex<Shell>>> {
    end_stale();
    shells()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id)
        .cloned()
        .with_context(|| format!("there is no open dev shell {id:?} (it was closed, or the mode changed or ran out); open one with dev_shell"))
}

fn exists(id: &str) -> bool {
    get(id).is_ok()
}

/// Developer mode must be on: the mode in force.
fn mode_on() -> Result<Mode> {
    let mode = dev::mode();
    ensure!(mode != Mode::Safe, "developer mode is off; only the person can turn it on, with `orion mode dev`");
    Ok(mode)
}

fn wait_for(policy: &Policy, asked: Option<u64>) -> Duration {
    let most = policy.dev.wait_secs.max(1);
    Duration::from_secs(asked.unwrap_or(60).clamp(0, most))
}

pub struct DevShell;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellArgs {
    command: String,
    /// An open shell; without it, a new one opens.
    shell: Option<String>,
    wait_secs: Option<u64>,
}

impl Tool for DevShell {
    fn name(&self) -> &'static str {
        "dev_shell"
    }

    fn description(&self) -> &'static str {
        "Developer mode only: run a bash command line in a persistent shell, for building, testing and debugging \
         software in the dev root (~/orion-dev). The shell stays open between calls, so cd, exported variables, \
         activated virtual environments and background jobs carry over; pass the `shell` id a call returned to keep \
         using it, or leave it out to open a new one (the person approves that). Pipes, redirects and globs work. \
         Commands read no input, and there is no terminal, so give interactive tools their non-interactive flags. A \
         command still running after wait_secs keeps running: follow it with dev_shell_read. The dev root is \
         checkpointed before each command, so dev_rollback can undo what one did there. In sandboxed developer mode \
         the network is on, and only the dev root and the toolchain caches can be written."
    }

    fn risk(&self) -> Risk {
        Risk::Execute
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "A bash command line." },
                "shell": { "type": "string", "description": "The id of an open shell, from an earlier dev_shell result. Leave out to open one." },
                "wait_secs": { "type": "integer", "minimum": 0, "default": 60,
                               "description": "How long to wait for the command before returning; it keeps running after." }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    fn preapproval(&self, _policy: &Policy, args: &Value) -> Preapproval {
        match args.get("shell").and_then(Value::as_str) {
            Some(id) if exists(id) => Preapproval::Granted,
            _ => Preapproval::Person(opening(dev::mode()).into()),
        }
    }

    fn every_time(&self, args: &Value) -> bool {
        args.get("shell").and_then(Value::as_str).is_none_or(|id| !exists(id)) && dev::mode() == Mode::Unsandboxed
    }

    fn redact(&self, args: &Value) -> Value {
        let mut shown = args.clone();
        if args.get("shell").and_then(Value::as_str).is_none_or(|id| !exists(id)) {
            shown["opens"] = json!(opening(dev::mode()));
        }
        shown
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        let a: ShellArgs = parse_args(args.clone())?;
        let mode = mode_on()?;
        ensure!(!a.command.trim().is_empty(), "give a command");
        match &a.shell {
            Some(id) => get(id).map(drop),
            None => {
                ensure!(shells().lock().unwrap_or_else(|e| e.into_inner()).len() < MAX_SHELLS, "{MAX_SHELLS} dev shells are open; close one first");
                dev::shell_policy(policy, mode).map(drop)
            }
        }
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: ShellArgs = parse_args(args)?;
        let mode = mode_on()?;
        let (id, shell) = match &a.shell {
            Some(id) => (id.clone(), get(id)?),
            None => {
                let shell = Arc::new(Mutex::new(Shell::open(policy, mode)?));
                let id = format!("sh-{}", &random_hex()[..8]);
                shells().lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), shell.clone());
                (id, shell)
            }
        };
        let mut s = shell.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(s.running.is_none(), "shell {id} is still running a command; follow it with dev_shell_read, or close it");
        let checkpoint = Store::new(&policy.dev)?.take(&a.command)?;
        s.start(&a.command)?;
        let mut result = s.follow(wait_for(policy, a.wait_secs), policy.proc.max_output_bytes);
        result["shell"] = json!(id);
        result["mode"] = json!(s.mode);
        result["checkpoint"] = json!(checkpoint);
        Ok(result)
    }
}

/// What opening a shell means, for the person approving it.
fn opening(mode: Mode) -> &'static str {
    match mode {
        Mode::Unsandboxed => {
            "a new UNSANDBOXED dev shell: its commands run as you, with your files, keys and network, without asking \
             again (Orion's files, its sockets and root stay out of reach)"
        }
        _ => "a new sandboxed dev shell: its commands run without asking again, in ~/orion-dev, with the network",
    }
}

pub struct DevShellRead;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    shell: String,
    wait_secs: Option<u64>,
}

impl Tool for DevShellRead {
    fn name(&self) -> &'static str {
        "dev_shell_read"
    }

    fn description(&self) -> &'static str {
        "Developer mode only: what a dev shell's running command printed since last shown, waiting up to wait_secs \
         for it to finish; the exit code once it has."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "shell": { "type": "string" },
                "wait_secs": { "type": "integer", "minimum": 0, "default": 60 }
            },
            "required": ["shell"],
            "additionalProperties": false
        })
    }

    fn precheck(&self, _policy: &Policy, args: &Value) -> Result<()> {
        let a: ReadArgs = parse_args(args.clone())?;
        mode_on()?;
        get(&a.shell).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: ReadArgs = parse_args(args)?;
        let shell = get(&a.shell)?;
        let mut s = shell.lock().unwrap_or_else(|e| e.into_inner());
        if s.running.is_none() {
            return Ok(json!({ "shell": a.shell, "running": false, "output": "", "note": "nothing is running in this shell" }));
        }
        let mut result = s.follow(wait_for(policy, a.wait_secs), policy.proc.max_output_bytes);
        result["shell"] = json!(a.shell);
        Ok(result)
    }
}

pub struct DevShellClose;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseArgs {
    shell: String,
}

impl Tool for DevShellClose {
    fn name(&self) -> &'static str {
        "dev_shell_close"
    }

    fn description(&self) -> &'static str {
        "Close a dev shell, stopping everything it runs (a server, a build that hangs)."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "shell": { "type": "string" } },
            "required": ["shell"],
            "additionalProperties": false
        })
    }

    fn preapproval(&self, _policy: &Policy, _args: &Value) -> Preapproval {
        // It only ends what developer mode started.
        Preapproval::Granted
    }

    fn precheck(&self, _policy: &Policy, args: &Value) -> Result<()> {
        let a: CloseArgs = parse_args(args.clone())?;
        get(&a.shell).map(drop)
    }

    fn call(&self, _policy: &Policy, args: Value) -> Result<Value> {
        let a: CloseArgs = parse_args(args)?;
        let shell = shells().lock().unwrap_or_else(|e| e.into_inner()).remove(&a.shell);
        match shell {
            Some(s) => {
                s.lock().unwrap_or_else(|e| e.into_inner()).kill();
                Ok(json!({ "shell": a.shell, "closed": true }))
            }
            None => bail!("there is no open dev shell {:?}", a.shell),
        }
    }
}

pub struct DevHistory;

impl Tool for DevHistory {
    fn name(&self) -> &'static str {
        "dev_history"
    }

    fn description(&self) -> &'static str {
        "The dev root's checkpoints, newest first: one before each dev_shell command, and before each rollback. \
         dev_rollback puts the dev root back to one."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": { "limit": { "type": "integer", "minimum": 1, "default": 20 } }, "additionalProperties": false })
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
        let list = Store::new(&policy.dev)?.list()?;
        Ok(json!({
            "root": policy.dev.root(),
            "checkpoints": list.iter().take(limit).map(|c| json!({
                "id": c.id,
                "taken": crate::tools::journal_time(c.ts as i64),
                "before": c.before,
                "files": c.files,
                "bytes": c.bytes,
            })).collect::<Vec<_>>(),
            "total": list.len(),
            "mode": dev::state().describe(),
        }))
    }
}

pub struct DevRollback;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RollbackArgs {
    id: u64,
}

impl Tool for DevRollback {
    fn name(&self) -> &'static str {
        "dev_rollback"
    }

    fn description(&self) -> &'static str {
        "Put the dev root back as a checkpoint found it (dev_history lists them): files changed since are restored, \
         files made since removed. Build output and dependency folders (target, node_modules, .venv, …) are left as \
         they are, as is everything outside the dev root. A checkpoint is taken first, so the rollback can be undone."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "integer", "minimum": 1 } },
            "required": ["id"],
            "additionalProperties": false
        })
    }

    fn precheck(&self, _policy: &Policy, args: &Value) -> Result<()> {
        let _: RollbackArgs = parse_args(args.clone())?;
        let busy = shells().lock().unwrap_or_else(|e| e.into_inner()).values().any(|s| s.try_lock().map_or(true, |s| s.running.is_some()));
        ensure!(!busy, "a dev shell is still running a command; let it finish, or close the shell, first");
        Ok(())
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: RollbackArgs = parse_args(args)?;
        let r = Store::new(&policy.dev)?.rollback(a.id)?;
        Ok(json!({ "rolled_back_to": a.id, "restored": r.restored, "removed": r.removed, "unchanged": r.unchanged, "undo_with": r.undo_with }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_and_encoding() {
        assert_eq!(base64(b"ls -la"), "bHMgLWxh");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        let out = b"hello\n\n__orion_done_ab 3 /home/me/orion-dev/app\n";
        assert_eq!(find_marker(out, "__orion_done_ab"), Some((7, 3, "/home/me/orion-dev/app".into())));
        assert_eq!(find_marker(b"hello\n__orion_done_ab 3", "__orion_done_ab"), None, "not until the line ends");
        assert_eq!(tail("abcdef".into(), 3), ("def".into(), true));

        // After the front of a long output was dropped.
        let out = Output { buf: b"...error: E0308\n\n__orion_done_ab 101 /x\n".to_vec(), base: 5000, closed: false };
        let (marker, rc, _) = out.marker(100, "__orion_done_ab").unwrap();
        assert_eq!((marker, rc), (5000 + 17, 101));
        assert_eq!(out.between(100, marker), b"...error: E0308\n\n");
        assert_eq!(out.between(5003, marker), b"error: E0308\n\n");
        assert_eq!(tail("ab".into(), 3), ("ab".into(), false));
    }
}
