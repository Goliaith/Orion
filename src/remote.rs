//! The chat, the shell and `orion listen` calling tools through the daemon,
//! so every interface and agent shares one policy, sandbox and audit log.
//!
//! They connect only when the daemon runs this same version of Orion with
//! the same policy file; otherwise they call tools in-process as before.
//! The chat answers approval prompts for its own calls: the daemon asks it
//! over the connection (`orion/approvals/self`), and the chat's own
//! approver (the prompt, the card) answers. Ctrl-C cancels the running call
//! in the daemon (`notifications/cancelled`).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::{interrupt, Approver, CallError, Orion, Policy};

struct Conn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    /// A line read in part before a read timed out.
    partial: String,
    next: u64,
}

impl Conn {
    fn send(&mut self, msg: &Value) -> Result<()> {
        writeln!(self.writer, "{msg}")?;
        self.writer.flush()?;
        Ok(())
    }

    /// The next message, or `None` if none came within the read timeout.
    fn read(&mut self) -> Result<Option<Value>> {
        match self.reader.read_line(&mut self.partial) {
            Ok(0) => bail!("the Orion daemon went away"),
            Ok(_) if self.partial.ends_with('\n') => {
                let line = std::mem::take(&mut self.partial);
                Ok(Some(serde_json::from_str(&line).context("the daemon's message")?))
            }
            Ok(_) => Ok(None),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Wait for the response to request `id`, ignoring everything else.
    fn response(&mut self, id: u64) -> Result<Value> {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            if let Some(msg) = self.read()? {
                if msg["id"] == id {
                    return Ok(msg);
                }
            }
        }
        bail!("the Orion daemon didn't answer")
    }
}

/// A connection to the daemon, for tool calls.
pub struct Remote {
    conn: Mutex<Conn>,
    pid: u64,
    dead: AtomicBool,
}

impl Remote {
    /// Connect, if the daemon runs and `[daemon] chat` allows it, with this
    /// version and this policy file. Why not, otherwise.
    pub fn connect(policy: &Policy, source: Option<&Path>, client: &str) -> Result<Self, String> {
        if policy.daemon.chat == "never" {
            return Err("[daemon] chat is \"never\"".into());
        }
        let st = crate::daemon::status(&policy.daemon).map_err(|_| "no daemon is running".to_string())?;
        if st["version"] != env!("CARGO_PKG_VERSION") {
            return Err(format!("the daemon is version {}, this is {}: restart it", st["version"], env!("CARGO_PKG_VERSION")));
        }
        let ours = source.map_or("built-in defaults".to_string(), |p| p.display().to_string());
        if st["policy"].as_str() != Some(&ours) {
            return Err(format!("the daemon uses another policy ({})", st["policy"].as_str().unwrap_or("?")));
        }
        let stream = crate::daemon::connect(&policy.daemon).ok_or("the daemon went away")?;
        Self::open(stream, st["pid"].as_u64().unwrap_or(0), client).map_err(|e| format!("{e:#}"))
    }

    fn open(stream: UnixStream, pid: u64, client: &str) -> Result<Self> {
        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
        let mut conn = Conn { reader: BufReader::new(stream.try_clone()?), writer: stream, partial: String::new(), next: 3 };
        conn.send(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": crate::mcp::PROTOCOL_VERSIONS[0], "clientInfo": { "name": client, "version": env!("CARGO_PKG_VERSION") } } }))?;
        conn.response(1)?;
        conn.send(&json!({ "jsonrpc": "2.0", "id": 2, "method": "orion/approvals/self" }))?;
        let r = conn.response(2)?;
        if let Some(e) = r.get("error") {
            bail!("the daemon can't ask this chat for approvals: {}", e["message"]);
        }
        Ok(Self { conn: Mutex::new(conn), pid, dead: AtomicBool::new(false) })
    }

    /// The daemon's process id.
    pub fn pid(&self) -> u64 {
        self.pid
    }

    pub fn alive(&self) -> bool {
        !self.dead.load(Ordering::Relaxed)
    }

    /// Call a tool in the daemon, answering its approval prompts with
    /// `approver`. `orion` names the tools for those prompts.
    pub fn call(&self, orion: &Orion, name: &str, args: Value, approver: Option<&dyn Approver>, caller: Option<&str>) -> Result<Value, CallError> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let result = call(&mut conn, orion, name, args, approver, caller);
        if let Err(CallError::Failed(e)) = &result {
            if e.to_string().contains("daemon went away") {
                self.dead.store(true, Ordering::Relaxed);
            }
        }
        result
    }
}

fn call(conn: &mut Conn, orion: &Orion, name: &str, args: Value, approver: Option<&dyn Approver>, caller: Option<&str>) -> Result<Value, CallError> {
    let id = conn.next;
    conn.next += 1;
    let mut params = json!({ "name": name, "arguments": args });
    if let Some(c) = caller {
        params["_meta"] = json!({ "orion/caller": c });
    }
    conn.send(&json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params })).map_err(CallError::Failed)?;
    let mut cancelled = false;
    loop {
        if interrupt::requested() && !cancelled {
            // The daemon stops the call and answers that it was interrupted.
            cancelled = true;
            let _ = conn.send(&json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": id } }));
        }
        let Some(msg) = conn.read().map_err(CallError::Failed)? else { continue };
        if msg["method"] == "orion/approval/request" {
            let p = &msg["params"];
            let tool = orion.tools().find(|(t, _)| Some(t.name()) == p["tool"].as_str()).map(|(t, _)| t);
            // The daemon sent the arguments with secrets blanked; the
            // approver shows them, so that is what it should see.
            let yes = match (tool, approver) {
                (Some(t), Some(a)) => a.approve(t, &p["args"]),
                _ => false,
            };
            let rid = conn.next;
            conn.next += 1;
            let answer = json!({ "jsonrpc": "2.0", "id": rid, "method": "orion/approvals/answer", "params": { "id": p["id"], "approve": yes } });
            conn.send(&answer).map_err(CallError::Failed)?;
            continue;
        }
        if msg["id"] != id {
            continue; // answers to our answers, resolved notices
        }
        if let Some(e) = msg.get("error") {
            let message = e["message"].as_str().unwrap_or("error").to_string();
            return Err(if message.starts_with("unknown tool") { CallError::Unknown(name.into()) } else { CallError::Failed(anyhow!(message)) });
        }
        let r = &msg["result"];
        let text = r.pointer("/content/0/text").and_then(Value::as_str).unwrap_or_default().to_string();
        if r["isError"] == true {
            return Err(match r.pointer("/_meta/orion~1error").and_then(Value::as_str) {
                Some("denied") => CallError::Denied(text),
                Some("interrupted") => CallError::Failed(interrupt::Interrupted.into()),
                _ => CallError::Failed(anyhow!(text)),
            });
        }
        return serde_json::from_str(&text).map_err(|e| CallError::Failed(anyhow!("the daemon's result: {e}")));
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::thread;

    use super::*;
    use crate::Tool;

    struct Answer {
        yes: bool,
        asked: Cell<u32>,
    }

    impl Approver for Answer {
        fn approve(&self, _tool: &dyn Tool, _args: &Value) -> bool {
            self.asked.set(self.asked.get() + 1);
            self.yes
        }
    }

    #[test]
    fn the_chat_calls_tools_through_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();
        let config = dir.path().join("orion.toml");
        let audit = dir.path().join("audit.jsonl");
        let socket = dir.path().join("run/orion.sock");
        std::fs::write(
            &config,
            format!(
                "[fs]\nwrite_roots = [{work:?}]\n[proc]\nallowed_commands = [\"sleep\"]\n[tools]\nproc_run = \"allow\"\n\
                 [audit]\npath = {audit:?}\n[sandbox]\nmode = \"off\"\n[daemon]\nsocket = {socket:?}\n"
            ),
        )
        .unwrap();
        let (policy, source) = Policy::load(Some(&config)).unwrap();
        {
            let (policy, source) = (policy.clone(), source.clone());
            thread::spawn(move || crate::daemon::run(policy, source, None));
        }
        let start = Instant::now();
        while UnixStream::connect(&socket).is_err() {
            assert!(start.elapsed() < Duration::from_secs(5), "daemon did not start");
            thread::sleep(Duration::from_millis(20));
        }

        let orion = Orion::new(policy.clone()).through_daemon(source.as_deref(), "test chat");
        assert!(orion.runs_tools().starts_with("through the daemon"), "{}", orion.runs_tools());
        assert!(orion.call("system_info", json!({}), None).unwrap()["hostname"].is_string());

        // The daemon asks the chat; the chat's approver answers.
        let file = work.join("note.txt");
        let yes = Answer { yes: true, asked: Cell::new(0) };
        orion.call_as("fs_write", json!({ "path": file, "content": "hi" }), Some(&yes), Some("ana (by voice)")).unwrap();
        assert_eq!((yes.asked.get(), std::fs::read_to_string(&file).unwrap().as_str()), (1, "hi"));
        let no = Answer { yes: false, asked: Cell::new(0) };
        let err = orion.call("fs_write", json!({ "path": file, "content": "bye" }), Some(&no)).unwrap_err();
        assert!(matches!(err, CallError::Denied(_)), "{err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hi");
        assert!(matches!(orion.call("no_such_tool", json!({}), None), Err(CallError::Unknown(_))));

        // One audit log, written by the daemon, naming who asked.
        let log = std::fs::read_to_string(&audit).unwrap();
        assert!(log.contains("\"caller\":\"ana (by voice) via test chat (pid"), "{log}");
        assert!(log.contains("\"approved\":false"), "{log}");

        // Ctrl-C in the chat stops the program in the daemon.
        let flag = interrupt::test_flag::install();
        let stopper = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            flag.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let err = orion.call("proc_run", json!({ "command": "sleep", "args": ["30"] }), None).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
        assert!(matches!(&err, CallError::Failed(e) if interrupt::is_interrupt(e)), "{err}");
        stopper.join().unwrap();
        interrupt::clear();

        // Another policy file: the chat keeps its tools to itself.
        let other = dir.path().join("other.toml");
        std::fs::write(&other, format!("[daemon]\nsocket = {socket:?}\n")).unwrap();
        let (p2, s2) = Policy::load(Some(&other)).unwrap();
        let alone = Orion::new(p2).through_daemon(s2.as_deref(), "test chat");
        assert!(alone.runs_tools().contains("another policy"), "{}", alone.runs_tools());
    }
}
