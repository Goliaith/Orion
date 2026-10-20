//! Approvals through the daemon: an `ask` call from an AI agent reaches a
//! person, wherever they are watching.
//!
//! The daemon keeps the calls waiting for an answer here. A connection
//! becomes an approver by sending `orion/approvals/watch` (`orion
//! approvals`, desktop notifications, the shell); it then receives each
//! request as an `orion/approval/request` notification and answers with
//! `orion/approvals/answer`. The first answer counts; the others hear
//! `orion/approval/resolved`. A chat connected to the daemon answers for its
//! own calls the same way (`orion/approvals/self`). Nobody answering within
//! `[daemon] approval_timeout_secs` counts as no.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::{Approver, Tool};

/// One connection's outgoing side, shared by every thread that answers on it.
pub type Writer = Arc<Mutex<UnixStream>>;

/// Write one JSON-RPC message as a line.
pub fn send(w: &Writer, msg: &Value) -> std::io::Result<()> {
    let mut s = w.lock().unwrap_or_else(|e| e.into_inner());
    writeln!(s, "{msg}")?;
    s.flush()
}

fn notify(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

struct Pending {
    request: Value,
    answer: Sender<bool>,
}

/// Who should answer a call.
pub enum Ask {
    /// Everyone watching (`orion approvals`, notifications, the shell).
    Watchers,
    /// The connection that made the call (a chat, answering for itself).
    Caller(Writer),
}

#[derive(Default)]
pub struct Broker {
    next: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    /// Connection id → its writer.
    watchers: Mutex<HashMap<u64, Writer>>,
    timeout_secs: AtomicU64,
}

impl Broker {
    pub fn new(timeout: Duration) -> Self {
        let b = Self::default();
        b.timeout_secs.store(timeout.as_secs().max(1), Ordering::Relaxed);
        b
    }

    /// Start sending requests to this connection. What already waits.
    pub fn watch(&self, conn: u64, w: Writer) -> Vec<Value> {
        self.watchers.lock().unwrap_or_else(|e| e.into_inner()).insert(conn, w);
        self.list()
    }

    pub fn unwatch(&self, conn: u64) {
        self.watchers.lock().unwrap_or_else(|e| e.into_inner()).remove(&conn);
    }

    pub fn watching(&self) -> usize {
        self.watchers.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn list(&self) -> Vec<Value> {
        let mut v: Vec<Value> = self.pending.lock().unwrap_or_else(|e| e.into_inner()).values().map(|p| p.request.clone()).collect();
        v.sort_by_key(|r| r["id"].as_u64());
        v
    }

    /// Answer request `id`. Whether it was still waiting.
    pub fn answer(&self, id: u64, approve: bool) -> bool {
        match self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
            Some(p) => p.answer.send(approve).is_ok(),
            None => false,
        }
    }

    /// Ask whoever `who` says, and wait for an answer.
    pub fn ask(&self, who: &Ask, tool: &dyn Tool, args: &Value, caller: Option<&str>) -> Outcome {
        let targets: Vec<Writer> = match who {
            Ask::Caller(w) => vec![w.clone()],
            Ask::Watchers => self.watchers.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect(),
        };
        if targets.is_empty() {
            return Outcome::NobodyWatching;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let timeout = self.timeout_secs.load(Ordering::Relaxed);
        let request = json!({
            "id": id,
            "tool": tool.name(),
            "description": tool.description(),
            "risk": format!("{:?}", tool.risk()).to_lowercase(),
            "args": tool.redact(args),
            "caller": caller,
            "timeout_secs": timeout,
        });
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, Pending { request: request.clone(), answer: tx });
        let delivered = targets.iter().filter(|w| send(w, &notify("orion/approval/request", request.clone())).is_ok()).count();
        let outcome = if delivered == 0 {
            Outcome::NobodyWatching
        } else {
            // A cancelled call stops waiting too.
            let until = std::time::Instant::now() + Duration::from_secs(timeout);
            loop {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(true) => break Outcome::Approved,
                    Ok(false) => break Outcome::Declined,
                    Err(mpsc::RecvTimeoutError::Timeout) if std::time::Instant::now() < until && !crate::interrupt::requested() => {}
                    _ => break Outcome::Unanswered,
                }
            }
        };
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        let word = match outcome {
            Outcome::Approved => "approved",
            Outcome::Declined => "declined",
            Outcome::Unanswered | Outcome::NobodyWatching => "unanswered",
        };
        for w in &targets {
            let _ = send(w, &notify("orion/approval/resolved", json!({ "id": id, "outcome": word })));
        }
        outcome
    }
}

/// How an approval request ended.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    Approved,
    Declined,
    /// Nobody answered in time.
    Unanswered,
    /// Nobody was watching to ask.
    NobodyWatching,
}

/// The daemon's approver for one call: asks through the broker.
pub struct BrokerApprover<'a> {
    pub broker: &'a Broker,
    pub who: Ask,
    pub caller: Option<String>,
    pub last: Mutex<Option<Outcome>>,
}

impl Approver for BrokerApprover<'_> {
    fn approve(&self, tool: &dyn Tool, args: &Value) -> bool {
        let outcome = self.broker.ask(&self.who, tool, args, self.caller.as_deref());
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
        outcome == Outcome::Approved
    }

    fn why_not(&self) -> Option<String> {
        match *self.last.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(Outcome::NobodyWatching) => Some(
                "this needs a person's approval, and nobody is approving right now; they can run `orion approvals` \
                 (or open the Orion shell) and you can try again"
                    .into(),
            ),
            Some(Outcome::Unanswered) => Some("nobody answered the approval request in time, so it was declined".into()),
            Some(Outcome::Declined) => Some("the person declined this call".into()),
            _ => None,
        }
    }
}

/// How `orion approvals` answers.
pub enum Mode {
    /// Ask in this terminal, one request at a time.
    Terminal,
    /// A desktop notification with Approve and Decline buttons per request.
    Notify,
    /// Print what waits, and exit.
    List,
}

/// `orion approvals`: watch the daemon for approval requests and answer them.
pub fn watch(config: &crate::daemon::DaemonConfig, mode: Mode) -> Result<()> {
    let path = crate::daemon::socket_path(config);
    let stream = UnixStream::connect(&path).with_context(|| {
        format!("no Orion daemon on {} (orion daemon, or systemctl --user start orion.socket)", path.display())
    })?;
    let w: Writer = Arc::new(Mutex::new(stream.try_clone()?));
    let hello = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": crate::mcp::PROTOCOL_VERSIONS[0], "clientInfo": { "name": "orion approvals", "version": env!("CARGO_PKG_VERSION") } } });
    send(&w, &hello)?;
    let method = if matches!(mode, Mode::List) { "orion/approvals/list" } else { "orion/approvals/watch" };
    send(&w, &json!({ "jsonrpc": "2.0", "id": 2, "method": method }))?;

    // Requests to answer, in order; the reader thread fills the queue.
    let (tx, rx) = mpsc::channel::<Value>();
    let resolved: Arc<Mutex<HashMap<u64, String>>> = Arc::default();
    let reader = {
        let resolved = resolved.clone();
        let list_only = matches!(mode, Mode::List);
        std::thread::spawn(move || -> Result<()> {
            for line in BufReader::new(stream).lines() {
                let msg: Value = serde_json::from_str(&line?)?;
                match (msg["id"].as_u64(), msg["method"].as_str()) {
                    (Some(2), None) => {
                        if let Some(e) = msg.get("error") {
                            bail!("the daemon refused: {} (is it an older version? restart it)", e["message"]);
                        }
                        let pending = msg["result"]["pending"].as_array().cloned().unwrap_or_default();
                        if list_only {
                            if pending.is_empty() {
                                println!("Nothing waits for approval.");
                            }
                            for r in &pending {
                                println!("{}", describe(r));
                            }
                            return Ok(());
                        }
                        for r in pending {
                            let _ = tx.send(r);
                        }
                    }
                    (_, Some("orion/approval/request")) => {
                        let _ = tx.send(msg["params"].clone());
                    }
                    (_, Some("orion/approval/resolved")) => {
                        if let (Some(id), Some(o)) = (msg["params"]["id"].as_u64(), msg["params"]["outcome"].as_str()) {
                            resolved.lock().unwrap_or_else(|e| e.into_inner()).insert(id, o.to_string());
                        }
                    }
                    _ => {}
                }
            }
            bail!("the daemon closed the connection")
        })
    };
    if matches!(mode, Mode::List) {
        return reader.join().unwrap_or_else(|_| bail!("reading the daemon's answer failed"));
    }
    eprintln!("Waiting for AI agents to ask (Ctrl-C to stop). Unanswered requests are declined after a while.");
    let notifier = matches!(mode, Mode::Notify);
    if notifier && !crate::tools::exec::installed("notify-send") {
        bail!("notify-send isn't installed (libnotify); run orion approvals without --notify to answer in this terminal");
    }
    let next_id = AtomicU64::new(100);
    let answer = |id: u64, approve: bool| {
        let rid = next_id.fetch_add(1, Ordering::Relaxed);
        let _ = send(&w, &json!({ "jsonrpc": "2.0", "id": rid, "method": "orion/approvals/answer", "params": { "id": id, "approve": approve } }));
    };
    loop {
        let r = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) if !reader.is_finished() => continue,
            Err(_) => return reader.join().unwrap_or_else(|_| bail!("the reader stopped")),
        };
        let Some(id) = r["id"].as_u64() else { continue };
        if resolved.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&id) {
            continue;
        }
        if notifier {
            // One thread per notification, so several can be open at once.
            let w = w.clone();
            std::thread::spawn(move || {
                if let Some(approve) = ask_by_notification(&r) {
                    let _ = send(&w, &json!({ "jsonrpc": "2.0", "id": 1_000_000 + id, "method": "orion/approvals/answer", "params": { "id": id, "approve": approve } }));
                }
            });
            continue;
        }
        println!("\n{}", describe(&r));
        let yes = ask_in_terminal(&format!("Approve {}? [y/N] ", r["tool"].as_str().unwrap_or("this call")));
        let gone = resolved.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned();
        match gone {
            Some(o) => println!("  (already {o} elsewhere)"),
            None => {
                answer(id, yes);
                println!("  {}", if yes { "approved" } else { "declined" });
            }
        }
    }
}

/// One request, for a person to read.
fn describe(r: &Value) -> String {
    let who = r["caller"].as_str().unwrap_or("an AI agent");
    let args = serde_json::to_string_pretty(&r["args"]).unwrap_or_default();
    let args: String = args.lines().map(|l| format!("    {l}\n")).collect();
    format!("{who} asks to run {} ({}):\n{args}  {}", r["tool"].as_str().unwrap_or("?"), r["risk"].as_str().unwrap_or("?"), r["description"].as_str().unwrap_or_default())
}

fn ask_in_terminal(prompt: &str) -> bool {
    use std::io::Write as _;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

/// A desktop notification with Approve and Decline, waiting for a click.
/// `None` when it was dismissed or expired without one.
fn ask_by_notification(r: &Value) -> Option<bool> {
    let who = r["caller"].as_str().unwrap_or("An AI agent");
    let summary = format!("{who} asks to run {}", r["tool"].as_str().unwrap_or("a tool"));
    let body: String = serde_json::to_string(&r["args"]).unwrap_or_default().chars().take(300).collect();
    let timeout_ms = (r["timeout_secs"].as_u64().unwrap_or(120) * 1000).to_string();
    let out = std::process::Command::new("notify-send")
        .args(["--app-name=Orion", "--urgency=critical", "--wait", "--expire-time", &timeout_ms])
        .args(["--action=approve=Approve", "--action=decline=Decline", "--", &summary, &body])
        .output()
        .ok()?;
    match String::from_utf8_lossy(&out.stdout).trim() {
        "approve" => Some(true),
        "decline" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn asks_watchers_and_takes_the_first_answer() {
        let broker = Arc::new(Broker::new(Duration::from_secs(5)));
        let tools = crate::tools::builtin();
        let tool = tools.iter().find(|t| t.name() == "wifi_connect").unwrap();
        // Nobody watching: nobody to ask.
        assert_eq!(broker.ask(&Ask::Watchers, tool.as_ref(), &json!({}), None), Outcome::NobodyWatching);

        let (ours, theirs) = UnixStream::pair().unwrap();
        assert!(broker.watch(7, Arc::new(Mutex::new(ours))).is_empty());
        assert_eq!(broker.watching(), 1);
        let b = broker.clone();
        let approver = std::thread::spawn(move || {
            let mut lines = BufReader::new(theirs).lines();
            let req: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
            assert_eq!(req["method"], "orion/approval/request");
            // Secrets stay hidden from approvers too.
            assert_eq!(req["params"]["args"]["password"], "••••••");
            assert_eq!(b.list().len(), 1);
            assert!(b.answer(req["params"]["id"].as_u64().unwrap(), true));
            let done: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
            assert_eq!(done["params"]["outcome"], "approved");
        });
        let args = json!({ "ssid": "Cafe", "password": "hunter2hunter2" });
        assert_eq!(broker.ask(&Ask::Watchers, tool.as_ref(), &args, Some("claude-code (pid 1)")), Outcome::Approved);
        approver.join().unwrap();
        assert!(broker.list().is_empty());
        assert!(!broker.answer(999, true), "nothing waits under that id");

        // Nobody answers in time: no.
        let quick = Broker::new(Duration::from_secs(1));
        let (ours, _theirs) = UnixStream::pair().unwrap();
        quick.watch(1, Arc::new(Mutex::new(ours)));
        assert_eq!(quick.ask(&Ask::Watchers, tool.as_ref(), &json!({}), None), Outcome::Unanswered);
    }
}
