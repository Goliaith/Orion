//! The audit log: one JSON line per tool call, each chained to the one
//! before by `prev`, the SHA-256 of the previous line. Editing, removing or
//! inserting a line breaks the chain at the next one, which `orion audit
//! --verify` finds. Rewriting the whole chain is caught by the checkpoints
//! the privileged helper keeps, where code running as you can't reach:
//! the daemon sends it the number of lines and the last line's hash as the
//! log grows, and `--verify` checks every checkpoint still holds.

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

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
        let mut line = json!({
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
        if let Err(e) = append(path, &mut line) {
            tracing::warn!("audit log {}: {e:#}", path.display());
        }
    }
}

/// SHA-256 of a line (without its newline), in hex.
pub fn hash(line: &str) -> String {
    Sha256::digest(line.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The file's last line, read from its end.
fn last_line(f: &mut fs::File) -> Result<Option<String>> {
    let len = f.seek(SeekFrom::End(0))?;
    let mut back = 64 << 10;
    loop {
        let start = len.saturating_sub(back);
        f.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        let text = String::from_utf8_lossy(&buf);
        let body = text.strip_suffix('\n').unwrap_or(&text);
        match body.rfind('\n') {
            Some(i) => return Ok(Some(body[i + 1..].to_string())),
            None if start == 0 => return Ok((!body.is_empty()).then(|| body.to_string())),
            None => back *= 4,
        }
    }
}

/// Append `line`, chained to the last one, under a lock: the daemon, a
/// chat and `orion call` may all write.
fn append(path: &Path, line: &mut Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut f = OpenOptions::new().create(true).read(true).append(true).open(path)?;
    // SAFETY: flock on a descriptor we own; released when `f` closes.
    ensure!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0, "locking the audit log");
    line["prev"] = json!(last_line(&mut f)?.map(|l| hash(&l)).unwrap_or_default());
    writeln!(f, "{line}")?;
    Ok(())
}

/// What `orion audit --verify` found.
#[derive(Debug, Default, PartialEq)]
pub struct Report {
    pub lines: usize,
    /// The first chained line (1-based); lines before it predate chaining.
    pub chained_from: Option<usize>,
    /// Where the chain breaks, and how.
    pub broken: Vec<String>,
    pub anchors: usize,
    pub anchors_bad: Vec<String>,
    pub last_anchor: Option<String>,
}

/// Check the chain of `text`, and the helper's `anchors` (JSONL) for `uid`.
pub fn check(text: &str, anchors: &str) -> Report {
    let lines: Vec<&str> = text.lines().collect();
    let mut r = Report { lines: lines.len(), ..Default::default() };
    for (i, line) in lines.iter().enumerate() {
        let n = i + 1;
        let prev = serde_json::from_str::<Value>(line).ok().and_then(|v| v["prev"].as_str().map(String::from));
        match (prev, r.chained_from) {
            (None, None) => {}
            (None, Some(_)) => r.broken.push(format!("entry {n} has no link to the one before: it was written by something other than Orion, or edited")),
            (Some(p), _) => {
                r.chained_from.get_or_insert(n);
                let want = if i == 0 { String::new() } else { hash(lines[i - 1]) };
                if p != want {
                    r.broken.push(format!("entry {n} doesn't follow entry {}: an entry was changed, removed or inserted there", n - 1));
                }
            }
        }
    }
    for a in anchors.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        let (Some(at), Some(h)) = (a["lines"].as_u64(), a["hash"].as_str()) else { continue };
        r.anchors += 1;
        let when = a["at"].as_str().unwrap_or("?").to_string();
        match lines.get((at as usize).wrapping_sub(1)) {
            Some(l) if hash(l) == h => {}
            Some(_) => r.anchors_bad.push(format!("the checkpoint of {when} (entry {at}) doesn't match: the log was rewritten up to there")),
            None => r.anchors_bad.push(format!("the checkpoint of {when} counts {at} entries, the log has {}: entries were removed", lines.len())),
        }
        r.last_anchor = Some(format!("{when} (entry {at})"));
    }
    r
}

/// The helper's checkpoints for this user, from `dir` (`[helper] anchors`).
pub fn anchors_path(dir: &str) -> PathBuf {
    // SAFETY: getuid has no preconditions.
    Path::new(dir).join(format!("{}.jsonl", unsafe { libc::getuid() }))
}

/// Ask the privileged helper to checkpoint the log as it stands, if it
/// grew since the last checkpoint. Nothing without the helper.
pub fn anchor(policy: &crate::Policy) -> Result<bool> {
    let Some(path) = policy.audit_path() else { return Ok(false) };
    let Ok(Some(crate::tools::exec::Elevation::Helper(socket))) = crate::tools::exec::elevation(policy) else { return Ok(false) };
    let Ok(text) = fs::read_to_string(&path) else { return Ok(false) };
    let lines: Vec<&str> = text.lines().collect();
    let Some(last) = lines.last() else { return Ok(false) };
    let done = fs::read_to_string(anchors_path(&policy.helper.anchors)).unwrap_or_default();
    let latest = done.lines().rev().filter_map(|l| serde_json::from_str::<Value>(l).ok()).find_map(|a| a["lines"].as_u64());
    if latest == Some(lines.len() as u64) {
        return Ok(false);
    }
    let r = crate::helper::run(&socket, crate::helper::ANCHOR, &[lines.len().to_string(), hash(last)], None, Duration::from_secs(10))?;
    if !r.success() {
        bail!("the helper didn't take the checkpoint: {}", r.complaint());
    }
    Ok(true)
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

    #[test]
    fn chains_entries_and_finds_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        // An entry from before chaining stays readable.
        fs::write(&path, "{\"tool\":\"old\"}\n").unwrap();
        let audit = Audit::new(Some(path.clone()));
        for tool in ["fs_read", "proc_run", "fs_write"] {
            audit.record(Entry { tool, caller: None, decision: Decision::Allow, approved: None, args: &json!({}), error: None, duration: Duration::ZERO });
        }
        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let anchor = |n: usize, line: &str| format!("{{\"at\":\"2026-10-24 09:00:00\",\"lines\":{n},\"hash\":\"{}\"}}\n", hash(line));
        let anchors = anchor(3, lines[2]);
        let r = check(&text, &anchors);
        assert_eq!((r.lines, r.chained_from, r.broken.len(), r.anchors, r.anchors_bad.len()), (4, Some(2), 0, 1, 0));

        // Edit an entry: the next one no longer follows.
        let edited = text.replace("\"tool\":\"proc_run\"", "\"tool\":\"fs_list\"");
        let r = check(&edited, &anchors);
        assert_eq!(r.broken, ["entry 4 doesn't follow entry 3: an entry was changed, removed or inserted there"]);
        assert_eq!(r.anchors_bad.len(), 1, "and the checkpoint at entry 3 no longer holds");
        // Remove the last two: the chain alone can't tell, the checkpoint can.
        let cut: String = lines[..2].iter().map(|l| format!("{l}\n")).collect();
        let r = check(&cut, &anchors);
        assert!(r.broken.is_empty());
        assert!(r.anchors_bad[0].contains("entries were removed"), "{:?}", r.anchors_bad);
        // Something else writes an entry without a link.
        let r = check(&format!("{text}{{\"tool\":\"forged\"}}\n"), "");
        assert!(r.broken[0].starts_with("entry 5 has no link"));
    }
}
