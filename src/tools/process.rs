use std::fs;
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

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
        let (mut cmd, sandbox, guard) = crate::sandbox::command(policy, &a.command, &a.args, cwd.as_deref())?;
        cmd.stdin(if a.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let timeout = Duration::from_secs(
            a.timeout_secs.unwrap_or(policy.proc.timeout_secs).clamp(1, policy.proc.timeout_secs.max(1)),
        );

        let mut child = cmd.spawn().with_context(|| format!("failed to start {:?}", a.command))?;
        drop(guard);
        let pgid = child.id() as libc::pid_t;
        if let (Some(input), Some(mut pipe)) = (a.stdin, child.stdin.take()) {
            thread::spawn(move || {
                let _ = pipe.write_all(input.as_bytes());
            });
        }
        let cap = policy.proc.max_output_bytes;
        let stdout = capture(child.stdout.take(), cap);
        let stderr = capture(child.stderr.take(), cap);

        let started = Instant::now();
        let mut timed_out = false;
        let mut interrupted = false;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() >= timeout || crate::interrupt::requested() {
                timed_out = !crate::interrupt::requested();
                interrupted = !timed_out;
                kill_group(pgid);
                break child.wait()?;
            }
            thread::sleep(Duration::from_millis(10));
        };
        // Kill leftover background children too: they would keep the output
        // pipes open and block the readers forever.
        kill_group(pgid);
        if interrupted {
            let _ = (stdout.join(), stderr.join());
            return Err(crate::interrupt::Interrupted.into());
        }

        let (stdout, stdout_truncated) = stdout.join().unwrap_or_default();
        let (stderr, stderr_truncated) = stderr.join().unwrap_or_default();
        Ok(json!({
            "exit_code": status.code(),
            "signal": status.signal(),
            "timed_out": timed_out,
            "duration_ms": started.elapsed().as_millis(),
            "stdout": stdout,
            "stderr": stderr,
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
            "sandbox": sandbox.name(),
        }))
    }
}

fn kill_group(pgid: libc::pid_t) {
    // SAFETY: kill(2) has no memory-safety preconditions; ESRCH is expected
    // when the group is already gone.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

/// Read a stream to EOF, keeping the first `cap` bytes.
fn capture(stream: Option<impl Read + Send + 'static>, cap: usize) -> JoinHandle<(String, bool)> {
    thread::spawn(move || {
        let Some(mut stream) = stream else { return (String::new(), false) };
        let mut kept = Vec::new();
        let mut truncated = false;
        let mut chunk = [0u8; 8192];
        while let Ok(n) = stream.read(&mut chunk) {
            if n == 0 {
                break;
            }
            let room = cap.saturating_sub(kept.len());
            kept.extend_from_slice(&chunk[..n.min(room)]);
            truncated |= n > room;
        }
        (String::from_utf8_lossy(&kept).into_owned(), truncated)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
