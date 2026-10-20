//! The system journal: what services, the kernel and programs logged.

use std::time::Duration;

use anyhow::{bail, ensure, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::exec;
use crate::policy::Policy;
use crate::tool::{parse_args, Risk, Tool};

pub struct JournalRead;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    unit: Option<String>,
    since: Option<String>,
    until: Option<String>,
    priority: Option<String>,
    /// 0: this boot, -1: the one before, and so on.
    boot: Option<i32>,
    grep: Option<String>,
    #[serde(default)]
    kernel: bool,
    #[serde(default)]
    user: bool,
    #[serde(default = "default_lines")]
    lines: usize,
}

fn default_lines() -> usize {
    100
}

const PRIORITIES: [&str; 8] = ["emerg", "alert", "crit", "err", "warning", "notice", "info", "debug"];

/// A time as journalctl takes it: "1h ago", "today", "-2d", "2026-10-20
/// 09:00". It goes in as `--since=…`, so it can't be read as an option.
fn check_time(t: &str) -> Result<&str> {
    let ok = !t.is_empty() && t.len() <= 40 && t.chars().all(|c| c.is_ascii_alphanumeric() || " :.+-".contains(c));
    ensure!(ok, "{t:?} isn't a time (like \"1h ago\", \"today\" or \"2026-10-20 09:00\")");
    Ok(t)
}

/// `YYYY-MM-DD HH:MM:SS` in local time.
pub(super) fn local_time(secs: i64) -> String {
    // SAFETY: localtime_r writes only into `tm`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = secs as libc::time_t;
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return secs.to_string();
    }
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// One `journalctl -o json` line, as an entry. A MESSAGE that isn't text
/// arrives as an array of bytes.
fn entry(line: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(line).ok()?;
    let field = |k: &str| v.get(k).and_then(Value::as_str);
    let message = match v.get("MESSAGE") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(bytes)) => {
            String::from_utf8_lossy(&bytes.iter().filter_map(|b| b.as_u64().map(|b| b as u8)).collect::<Vec<_>>()).into_owned()
        }
        _ => String::new(),
    };
    let message: String = if message.chars().count() > 2000 { message.chars().take(2000).chain("…".chars()).collect() } else { message };
    let usecs: i64 = field("__REALTIME_TIMESTAMP").and_then(|t| t.parse().ok()).unwrap_or(0);
    let priority = field("PRIORITY").and_then(|p| p.parse::<usize>().ok()).and_then(|p| PRIORITIES.get(p).copied());
    Some(json!({
        "time": local_time(usecs / 1_000_000),
        "unit": field("_SYSTEMD_UNIT").or(field("_SYSTEMD_USER_UNIT")),
        "source": field("SYSLOG_IDENTIFIER").or(field("_COMM")),
        "pid": field("_PID").and_then(|p| p.parse::<u32>().ok()),
        "priority": priority,
        "message": message,
    }))
}

impl Tool for JournalRead {
    fn name(&self) -> &'static str {
        "journal_read"
    }

    fn description(&self) -> &'static str {
        "Read the system journal (systemd's log): what services, the kernel and programs logged, newest last. \
         Narrow it by unit, time (since/until, like \"1h ago\" or \"today\"), priority (\"err\" shows errors and \
         worse), boot (0 this one, -1 the one before), kernel messages only, or text to look for (grep, ignoring case)."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "unit": { "type": "string", "description": "A systemd unit, such as \"ollama\" or \"nginx.service\"." },
                "since": { "type": "string", "description": "\"1h ago\", \"today\", \"yesterday\", \"2026-10-20 09:00\"." },
                "until": { "type": "string" },
                "priority": { "type": "string", "enum": PRIORITIES, "description": "This priority and worse." },
                "boot": { "type": "integer", "maximum": 0, "description": "0: this boot; -1: the previous one." },
                "grep": { "type": "string", "description": "Only entries containing this text, ignoring case." },
                "kernel": { "type": "boolean", "default": false, "description": "Only kernel messages." },
                "user": { "type": "boolean", "default": false, "description": "Your own user services' journal." },
                "lines": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 100 }
            },
            "additionalProperties": false
        })
    }

    fn call(&self, _policy: &Policy, args: Value) -> Result<Value> {
        let a: ReadArgs = parse_args(args)?;
        ensure!((1..=1000).contains(&a.lines), "lines is 1 to 1000");
        if !exec::installed("journalctl") {
            bail!("there is no systemd journal here (journalctl isn't installed); logs may be in /var/log, for fs_read");
        }
        let grep = a.grep.as_ref().map(|g| g.to_lowercase());
        ensure!(grep.as_ref().is_none_or(|g| !g.is_empty() && g.len() <= 200), "grep is 1 to 200 characters");
        // With grep, fetch more and filter here: journalctl's own --grep
        // needs a build with PCRE2, which not every distribution has.
        let fetch = if grep.is_some() { (a.lines * 50).min(50_000) } else { a.lines };
        let mut argv: Vec<String> = vec!["--no-pager".into(), "-o".into(), "json".into(), "-n".into(), fetch.to_string()];
        if let Some(t) = &a.since {
            argv.push(format!("--since={}", check_time(t)?));
        }
        if let Some(t) = &a.until {
            argv.push(format!("--until={}", check_time(t)?));
        }
        if let Some(u) = &a.unit {
            argv.push(format!("--unit={}", super::services::unit_name(u)?));
        }
        if let Some(p) = &a.priority {
            ensure!(PRIORITIES.contains(&p.as_str()), "priority is one of {}", PRIORITIES.join(", "));
            argv.push(format!("--priority={p}"));
        }
        if let Some(b) = a.boot {
            ensure!(b <= 0, "boot is 0 (this one) or negative (earlier ones)");
            argv.push(format!("--boot={b}"));
        }
        if a.kernel {
            argv.push("--dmesg".into());
        }
        if a.user {
            argv.push("--user".into());
        }
        let done = exec::run(exec::command_as(None, "journalctl", &argv), None, Duration::from_secs(60), 32 << 20)?;
        ensure!(done.success() || !done.stdout.trim().is_empty(), "journalctl: {}", done.complaint());
        let mut entries: Vec<Value> = done
            .stdout
            .lines()
            .filter_map(entry)
            .filter(|e| grep.as_ref().is_none_or(|g| e["message"].as_str().unwrap_or_default().to_lowercase().contains(g)))
            .collect();
        let matched = entries.len();
        entries.drain(..matched.saturating_sub(a.lines));
        let mut out = json!({ "count": matched.min(a.lines), "more": matched > a.lines, "entries": entries });
        let hint = done.stderr.to_lowercase();
        if hint.contains("not seeing messages from other users") || hint.contains("insufficient permissions") {
            out["note"] = json!("only your own messages are shown: reading the whole journal needs the systemd-journal (or adm) group");
        }
        if done.stdout_truncated {
            out["note"] = json!("the journal had more than Orion reads at once; narrow it with since, unit or priority");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_journal_entries() {
        let e = entry(
            r#"{"__REALTIME_TIMESTAMP":"1792468745214000","_SYSTEMD_UNIT":"ollama.service","SYSLOG_IDENTIFIER":"ollama","_PID":"812","PRIORITY":"3","MESSAGE":"CUDA error: out of memory"}"#,
        )
        .unwrap();
        assert_eq!(e["unit"], "ollama.service");
        assert_eq!(e["priority"], "err");
        assert_eq!(e["pid"], 812);
        assert_eq!(e["message"], "CUDA error: out of memory");
        assert!(e["time"].as_str().unwrap().starts_with("2026-"));
        // Binary messages come as bytes.
        let b = entry(r#"{"__REALTIME_TIMESTAMP":"0","MESSAGE":[104,105,255],"_COMM":"x"}"#).unwrap();
        assert_eq!(b["message"], "hi\u{fffd}");
        assert_eq!(b["source"], "x");
        assert!(entry("not json").is_none());

        for ok in ["1h ago", "today", "2026-10-20 09:00", "-2d"] {
            assert!(check_time(ok).is_ok(), "{ok}");
        }
        for bad in ["", "now; rm -rf /", "$(id)", "a\nb"] {
            assert!(check_time(bad).is_err(), "{bad:?}");
        }
    }
}
