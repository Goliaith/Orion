use std::fs;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::exec::glob_match;
use crate::policy::Policy;
use crate::tool::{parse_args, Preapproval, Risk, Tool};

pub struct ProcList;

#[derive(Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
enum SortBy {
    #[default]
    Pid,
    Memory,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    /// Case-insensitive substring of the name or command line.
    filter: Option<String>,
    #[serde(default)]
    sort: SortBy,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    200
}

struct ProcEntry {
    pid: u32,
    ppid: Option<u32>,
    name: String,
    state: String,
    rss_kb: Option<u64>,
    cmdline: String,
}

fn read_proc(pid: u32) -> Option<ProcEntry> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The name is in parentheses and may itself contain spaces or ')'.
    let (open, close) = (stat.find('(')?, stat.rfind(')')?);
    let name = stat[open + 1..close].to_string();
    let mut rest = stat[close + 1..].split_whitespace();
    let state = rest.next()?.to_string();
    let ppid = rest.next().and_then(|v| v.parse().ok());
    let rss_kb = fs::read_to_string(format!("/proc/{pid}/status")).ok().and_then(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.split_whitespace().next()?.parse().ok())
    });
    let cmdline = fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| {
            String::from_utf8_lossy(&b)
                .split('\0')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    Some(ProcEntry { pid, ppid, name, state, rss_kb, cmdline })
}

impl Tool for ProcList {
    fn name(&self) -> &'static str {
        "proc_list"
    }

    fn description(&self) -> &'static str {
        "List running processes (pid, parent, name, state, resident memory, command line). \
         Can be filtered by name or command line and sorted by pid or by memory."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "filter": { "type": "string", "description": "Case-insensitive substring of name or command line." },
                "sort": { "type": "string", "enum": ["pid", "memory"], "default": "pid" },
                "limit": { "type": "integer", "minimum": 1, "default": 200 }
            },
            "additionalProperties": false
        })
    }

    fn call(&self, _policy: &Policy, args: Value) -> Result<Value> {
        let a: ListArgs = parse_args(args)?;
        let filter = a.filter.map(|f| f.to_lowercase());
        let mut procs: Vec<ProcEntry> = fs::read_dir("/proc")
            .context("reading /proc")?
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
            .filter_map(read_proc)
            .filter(|p| match &filter {
                Some(f) => p.name.to_lowercase().contains(f) || p.cmdline.to_lowercase().contains(f),
                None => true,
            })
            .collect();
        match a.sort {
            SortBy::Pid => procs.sort_by_key(|p| p.pid),
            SortBy::Memory => procs.sort_by_key(|p| std::cmp::Reverse(p.rss_kb.unwrap_or(0))),
        }
        let total = procs.len();
        procs.truncate(a.limit);
        Ok(json!({
            "total": total,
            "truncated": total > procs.len(),
            "processes": procs
                .into_iter()
                .map(|p| json!({
                    "pid": p.pid,
                    "ppid": p.ppid,
                    "name": p.name,
                    "state": p.state,
                    "rss_kb": p.rss_kb,
                    "cmdline": p.cmdline,
                }))
                .collect::<Vec<_>>(),
        }))
    }
}

pub struct ProcRun;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArgs {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<String>,
    stdin: Option<String>,
    timeout_secs: Option<u64>,
}

impl Tool for ProcRun {
    fn name(&self) -> &'static str {
        "proc_run"
    }

    fn description(&self) -> &'static str {
        "Run a program with arguments and capture its exit status, stdout and stderr. No shell is \
         involved, so pipes, globs and redirects are not interpreted. Programs outside the policy's \
         proc.allowed_commands need the user's approval. Programs run sandboxed: they can write only in \
         the writable roots and temporary directories, private paths are hidden, and the network is off \
         unless the owner allowed it. When it finishes or times out, its whole process group is killed."
    }

    fn risk(&self) -> Risk {
        Risk::Execute
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Program name (looked up on PATH) or absolute path." },
                "args": { "type": "array", "items": { "type": "string" }, "default": [] },
                "cwd": { "type": "string", "description": "Working directory; must be inside a readable root." },
                "stdin": { "type": "string", "description": "Text written to the program's standard input." },
                "timeout_secs": { "type": "integer", "minimum": 1, "description": "Capped by the policy's proc.timeout_secs." }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    fn preapproval(&self, policy: &Policy, args: &Value) -> Preapproval {
        let command = args.get("command").and_then(Value::as_str).unwrap_or_default();
        if policy.command_allowed(command) {
            Preapproval::Granted
        } else {
            Preapproval::Refused(format!("{command:?} is not in proc.allowed_commands"))
        }
    }

    fn precheck(&self, _policy: &Policy, args: &Value) -> Result<()> {
        let a: RunArgs = parse_args(args.clone())?;
        if let Some(tool) = super::flatpak::redirect(&a.command, &a.args) {
            anyhow::bail!(
                "proc_run programs have no network or root, so installs fail in the sandbox; use {tool} instead, \
                 which runs outside it after approval"
            );
        }
        Ok(())
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: RunArgs = parse_args(args)?;
        let cwd = match &a.cwd {
            Some(cwd) => {
                let dir = policy.check_read(cwd)?;
                ensure!(dir.is_dir(), "{} is not a directory", dir.display());
                Some(dir)
            }
            None => None,
        };
        let (cmd, sandbox, guard) = crate::sandbox::command(policy, &a.command, &a.args, cwd.as_deref())?;
        let timeout = Duration::from_secs(
            a.timeout_secs.unwrap_or(policy.proc.timeout_secs).clamp(1, policy.proc.timeout_secs.max(1)),
        );
        let done = super::exec::run(cmd, a.stdin, timeout, policy.proc.max_output_bytes)?;
        drop(guard);
        Ok(json!({
            "exit_code": done.status.code(),
            "signal": done.signal(),
            "timed_out": done.timed_out,
            "duration_ms": done.duration.as_millis(),
            "stdout": done.stdout,
            "stderr": done.stderr,
            "stdout_truncated": done.stdout_truncated,
            "stderr_truncated": done.stderr_truncated,
            "sandbox": sandbox.name(),
        }))
    }
}

pub struct ProcSignal;

#[derive(Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(rename_all = "lowercase")]
enum Signal {
    #[default]
    Term,
    Kill,
    Int,
    Hup,
    Stop,
    Cont,
    Usr1,
    Usr2,
}

impl Signal {
    fn number(self) -> i32 {
        match self {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
            Signal::Int => libc::SIGINT,
            Signal::Hup => libc::SIGHUP,
            Signal::Stop => libc::SIGSTOP,
            Signal::Cont => libc::SIGCONT,
            Signal::Usr1 => libc::SIGUSR1,
            Signal::Usr2 => libc::SIGUSR2,
        }
    }

    /// Whether the process is meant to go away.
    fn ends(self) -> bool {
        matches!(self, Signal::Term | Signal::Kill | Signal::Int)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignalArgs {
    pid: Option<u32>,
    /// The program's name, as proc_list shows it.
    name: Option<String>,
    #[serde(default)]
    signal: Signal,
    /// With `name`: every process of that name, not just one.
    #[serde(default)]
    all: bool,
    #[serde(default = "default_wait")]
    wait_secs: u64,
    /// After `term` or `int`, `kill` what is still running when the wait is over.
    #[serde(default)]
    escalate: bool,
}

fn default_wait() -> u64 {
    5
}

/// A process `proc_signal` may act on: (pid, name, its systemd unit).
struct Target {
    pid: u32,
    name: String,
    unit: Option<String>,
}

/// Orion's own process and the ones it runs under (the shell, the chat,
/// the terminal): stopping those would stop Orion.
fn own_lineage() -> Vec<u32> {
    let mut out = vec![std::process::id()];
    while let Some(ppid) = read_proc(*out.last().unwrap_or(&1)).and_then(|p| p.ppid) {
        if ppid <= 1 || out.contains(&ppid) {
            break;
        }
        out.push(ppid);
    }
    out
}

fn check_target(policy: &Policy, pid: u32) -> Result<Target> {
    let p = read_proc(pid).with_context(|| format!("there is no process {pid}"))?;
    ensure!(pid > 1, "PID 1 runs the whole system; Orion never signals it");
    // Kernel threads have no program file and descend from kthreadd (PID 2).
    ensure!(pid != 2 && p.ppid != Some(2), "{pid} ({}) is a kernel thread", p.name);
    ensure!(!own_lineage().contains(&pid), "{pid} ({}) is Orion itself, or what it runs under", p.name);
    if let Some(pat) = policy.proc.protected.iter().find(|pat| glob_match(pat, &p.name)) {
        bail!("{pid} ({}) is protected (proc.protected: {pat:?}); no approval can stop it", p.name);
    }
    let unit = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok().as_deref().and_then(super::updates::unit_of).map(|(u, _)| u);
    if let Some(u) = &unit {
        if let Some(pat) = policy.services.protected.iter().find(|pat| glob_match(pat, u)) {
            bail!("{pid} ({}) belongs to {u}, which is protected (services.protected: {pat:?})", p.name);
        }
    }
    // SAFETY: getuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    if me != 0 {
        let owner = fs::metadata(format!("/proc/{pid}")).map(|m| std::os::unix::fs::MetadataExt::uid(&m)).unwrap_or(0);
        ensure!(owner == me, "{pid} ({}) belongs to another user", p.name);
    }
    Ok(Target { pid, name: p.name, unit })
}

fn targets(policy: &Policy, a: &SignalArgs) -> Result<Vec<Target>> {
    match (a.pid, &a.name) {
        (Some(pid), None) => Ok(vec![check_target(policy, pid)?]),
        (None, Some(name)) => {
            ensure!(!name.is_empty(), "name a process");
            let pids: Vec<u32> = all_pids().into_iter().filter(|&pid| read_proc(pid).is_some_and(|p| p.name == *name)).collect();
            ensure!(!pids.is_empty(), "no process is called {name:?} (proc_list shows the names)");
            ensure!(
                pids.len() == 1 || a.all,
                "{} processes are called {name:?} (PIDs {}); name one by pid, or set all",
                pids.len(),
                pids.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
            );
            ensure!(pids.len() <= 50, "{} processes are called {name:?}; that's more than Orion stops at once", pids.len());
            pids.into_iter().map(|pid| check_target(policy, pid)).collect()
        }
        _ => bail!("give either pid or name"),
    }
}

fn all_pids() -> Vec<u32> {
    fs::read_dir("/proc")
        .map(|d| d.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect())
        .unwrap_or_default()
}

/// Gone, or a zombie waiting for its parent: either way, it has exited.
fn exited(pid: u32) -> bool {
    read_proc(pid).is_none_or(|p| p.state == "Z")
}

fn send(pid: u32, signal: Signal) -> Result<()> {
    // SAFETY: kill(2) has no memory-safety preconditions.
    let r = unsafe { libc::kill(pid as libc::pid_t, signal.number()) };
    ensure!(r == 0, "signalling {pid}: {}", std::io::Error::last_os_error());
    Ok(())
}

fn wait_exit(pids: &[u32], secs: u64) {
    let until = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < until && !pids.iter().all(|&p| exited(p)) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

impl Tool for ProcSignal {
    fn name(&self) -> &'static str {
        "proc_signal"
    }

    fn description(&self) -> &'static str {
        "Stop, interrupt, pause or resume a process by pid or name: send it a signal (term by default), wait for it \
         to exit, and with `escalate` kill it if it doesn't. Never PID 1, kernel threads, Orion itself, or the \
         processes the session, network and machine stand on. For a process a systemd service runs, svc_control \
         is the better way; the result says which service it is."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pid": { "type": "integer", "minimum": 2 },
                "name": { "type": "string", "description": "The process name as proc_list shows it; give this or pid." },
                "signal": { "type": "string", "enum": ["term", "kill", "int", "hup", "stop", "cont", "usr1", "usr2"], "default": "term" },
                "all": { "type": "boolean", "default": false, "description": "With name: every process of that name." },
                "wait_secs": { "type": "integer", "minimum": 0, "maximum": 60, "default": 5 },
                "escalate": { "type": "boolean", "default": false, "description": "After term or int, kill what is still running when the wait is over." }
            },
            "additionalProperties": false
        })
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        targets(policy, &parse_args(args.clone())?).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: SignalArgs = parse_args(args)?;
        ensure!(a.wait_secs <= 60, "wait at most 60 seconds");
        let found = targets(policy, &a)?;
        for t in &found {
            send(t.pid, a.signal)?;
        }
        let pids: Vec<u32> = found.iter().map(|t| t.pid).collect();
        if a.signal.ends() {
            wait_exit(&pids, a.wait_secs);
        }
        let mut escalated = Vec::new();
        if a.escalate && a.signal != Signal::Kill && a.signal.ends() {
            for &pid in pids.iter().filter(|&&p| !exited(p)) {
                if send(pid, Signal::Kill).is_ok() {
                    escalated.push(pid);
                }
            }
            wait_exit(&escalated, 2);
        }
        Ok(json!({
            "signal": format!("{:?}", a.signal).to_uppercase(),
            "processes": found.iter().map(|t| json!({
                "pid": t.pid,
                "name": t.name,
                "exited": a.signal.ends().then(|| exited(t.pid)),
                "killed_after_wait": escalated.contains(&t.pid),
                "service": t.unit,
                "note": t.unit.as_ref().filter(|u| u.ends_with(".service")).map(|u| format!("{u} may start it again; svc_control stops a service for good")),
            })).collect::<Vec<_>>(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Instant;

    use super::*;

    #[test]
    fn signals_processes_but_never_the_ones_that_matter() {
        let p = Policy::from_toml("").unwrap();
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let r = ProcSignal.call(&p, json!({ "pid": pid, "wait_secs": 5 })).unwrap();
        assert_eq!(r["processes"][0]["exited"], true, "{r}");
        assert_eq!(r["signal"], "TERM");
        child.wait().unwrap();

        // One that ignores TERM: escalation kills it after the wait.
        let mut stubborn = std::process::Command::new("sh").args(["-c", "trap '' TERM; exec sleep 30"]).spawn().unwrap();
        // Once it is sleep, the shell has set the trap and exec'd.
        let until = Instant::now() + Duration::from_secs(5);
        while read_proc(stubborn.id()).is_none_or(|p| p.name != "sleep") && Instant::now() < until {
            thread::sleep(Duration::from_millis(10));
        }
        let r = ProcSignal.call(&p, json!({ "pid": stubborn.id(), "wait_secs": 1, "escalate": true })).unwrap();
        assert_eq!(r["processes"][0]["killed_after_wait"], true, "{r}");
        assert_eq!(r["processes"][0]["exited"], true, "{r}");
        stubborn.wait().unwrap();

        let refused = |a: Value| ProcSignal.precheck(&p, &a).unwrap_err().to_string();
        assert!(refused(json!({ "pid": 1 })).contains("PID 1"));
        assert!(refused(json!({ "pid": std::process::id() })).contains("Orion itself"));
        assert!(refused(json!({ "pid": 4_000_000 })).contains("no process"));
        assert!(refused(json!({ "name": "orion-no-such-process" })).contains("no process"));
        assert!(refused(json!({})).contains("either pid or name"));
        let strict = Policy::from_toml("[proc]\nprotected = [\"sle*\"]\n").unwrap();
        let mut guarded = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        // Until the exec is done, the child still has this test's name.
        let until = Instant::now() + Duration::from_secs(5);
        while read_proc(guarded.id()).is_none_or(|p| p.name != "sleep") && Instant::now() < until {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ProcSignal.precheck(&strict, &json!({ "pid": guarded.id() })).unwrap_err().to_string().contains("protected"));
        guarded.kill().unwrap();
        guarded.wait().unwrap();
    }

    fn policy(commands: &str) -> Policy {
        Policy::from_toml(&format!("[proc]\nallowed_commands = {commands}\ntimeout_secs = 2\nmax_output_bytes = 8\n"))
            .unwrap()
    }

    #[test]
    fn captures_output() {
        let p = policy(r#"["sh"]"#);
        assert!(matches!(ProcRun.preapproval(&p, &json!({ "command": "ls" })), Preapproval::Refused(_)));
        assert!(matches!(ProcRun.preapproval(&p, &json!({ "command": "sh" })), Preapproval::Granted));

        let r = ProcRun
            .call(&p, json!({ "command": "sh", "args": ["-c", "cat; echo err >&2; exit 3"], "stdin": "0123456789" }))
            .unwrap();
        assert_eq!(r["exit_code"], 3);
        assert_eq!(r["stdout"], "01234567");
        assert_eq!(r["stdout_truncated"], true);
        assert_eq!(r["stderr"], "err\n");
    }

    #[test]
    fn times_out_and_reaps_background_children() {
        let p = policy(r#"["*"]"#);
        let r = ProcRun.call(&p, json!({ "command": "sleep", "args": ["30"], "timeout_secs": 1 })).unwrap();
        assert_eq!(r["timed_out"], true);
        assert_eq!(r["signal"], libc::SIGKILL);

        // The background `sleep` inherits stdout; the call must still return.
        let start = Instant::now();
        let r = ProcRun.call(&p, json!({ "command": "sh", "args": ["-c", "sleep 30 & echo ok"] })).unwrap();
        assert_eq!(r["stdout"], "ok\n");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn ctrl_c_stops_the_program() {
        let flag = crate::interrupt::test_flag::install();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let start = Instant::now();
        let p = policy(r#"["*"]"#);
        let err = ProcRun.call(&p, json!({ "command": "sleep", "args": ["30"] })).unwrap_err();
        assert!(crate::interrupt::is_interrupt(&err));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn lists_this_process() {
        let me = std::process::id();
        let r = ProcList.call(&policy("[]"), json!({ "sort": "memory", "limit": 100000 })).unwrap();
        assert!(r["processes"].as_array().unwrap().iter().any(|p| p["pid"] == me));
    }
}
