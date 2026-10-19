use std::fs;
use std::time::Duration;

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

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Instant;

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
