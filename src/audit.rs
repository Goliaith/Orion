use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::policy::Decision;

/// Strings in logged arguments are cut to this many bytes, so a large
/// `fs_write` does not copy the whole file into the log.
const MAX_LOGGED_STRING: usize = 512;

pub struct Entry<'a> {
    pub tool: &'a str,
    /// Who asked, when known (an MCP client and its process).
    pub caller: Option<&'a str>,
    pub decision: Decision,
    /// The human's answer, for calls under `ask` that prompted one.
    pub approved: Option<bool>,
    pub args: &'a Value,
    pub error: Option<String>,
    pub duration: Duration,
}

/// Append-only JSONL log of every tool call.
pub struct Audit {
    path: Option<PathBuf>,
    lock: Mutex<()>,
}

impl Audit {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self { path, lock: Mutex::new(()) }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn record(&self, entry: Entry<'_>) {
        let Some(path) = &self.path else { return };
        let ts_ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let line = json!({
            "ts_ms": ts_ms,
            "tool": entry.tool,
            "caller": entry.caller,
            "decision": entry.decision,
            "approved": entry.approved,
            "ok": entry.error.is_none(),
            "error": entry.error,
            "duration_ms": entry.duration.as_millis(),
            "args": truncate_strings(entry.args),
        });
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = append(path, &line) {
            tracing::warn!("audit log {}: {e:#}", path.display());
        }
    }
}

fn append(path: &Path, line: &Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

fn truncate_strings(v: &Value) -> Value {
    match v {
        Value::String(s) if s.len() > MAX_LOGGED_STRING => {
            let mut end = MAX_LOGGED_STRING;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            Value::String(format!("{}…[{} bytes]", &s[..end], s.len()))
        }
        Value::Array(a) => Value::Array(a.iter().map(truncate_strings).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), truncate_strings(v))).collect()),
        other => other.clone(),
    }
}

/// The last `n` lines of the log at `path`.
pub fn tail(path: &Path, n: usize) -> Result<Vec<String>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let lines: Vec<&str> = text.lines().collect();
    Ok(lines[lines.len().saturating_sub(n)..].iter().map(|s| s.to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/audit.jsonl");
        let audit = Audit::new(Some(path.clone()));
        let args = json!({ "path": "/x", "content": "é".repeat(400) });
        audit.record(Entry {
            tool: "fs_write",
            caller: Some("test"),
            decision: Decision::Deny,
            approved: None,
            args: &args,
            error: Some("denied".into()),
            duration: Duration::from_millis(3),
        });
        let lines = tail(&path, 10).unwrap();
        assert_eq!(lines.len(), 1);
        let v: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v["tool"], "fs_write");
        assert_eq!(v["decision"], "deny");
        assert_eq!(v["ok"], false);
        assert!(v["args"]["content"].as_str().unwrap().ends_with("…[800 bytes]"));
    }
}
