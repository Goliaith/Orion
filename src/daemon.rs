//! `orion daemon`: Orion as a long-running service. AI agents connect over
//! a Unix socket that only this user can reach, and every connection is an
//! MCP session against the same tools, policy and audit log. The policy is
//! reloaded when its file changes.
//!
//! `orion serve` bridges stdio to the daemon when one is running, so MCP
//! clients configured to launch `orion serve` share it automatically.

use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::approvals::{self, Ask, BrokerApprover, Broker, Writer};
use crate::mcp::{Extension, Server, Shared};
use crate::policy::expand_home;
use crate::{Decision, Orion, Policy};

/// `[daemon]` in orion.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// Empty: `$XDG_RUNTIME_DIR/orion/orion.sock`.
    pub socket: String,
    /// Offer agents the tools that need approval, and ask whoever watches
    /// (`orion approvals`, notifications, the shell). Off: agents get only
    /// `allow` tools, as without the daemon.
    pub approvals: bool,
    /// How long a call waits for an answer before it counts as no.
    pub approval_timeout_secs: u64,
    /// The chat, the shell and `orion listen` call tools through the daemon
    /// when it runs with the same policy (`auto`), or never (`never`).
    pub chat: String,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self { socket: String::new(), approvals: true, approval_timeout_secs: 120, chat: "auto".into() }
    }
}

pub fn socket_path(config: &DaemonConfig) -> PathBuf {
    if !config.socket.is_empty() {
        return expand_home(Path::new(&config.socket));
    }
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("orion/orion.sock"),
        // SAFETY: getuid never fails.
        None => PathBuf::from(format!("/tmp/orion-{}/orion.sock", unsafe {
            libc::getuid()
        })),
    }
}

/// The daemon, if one is listening.
pub fn connect(config: &DaemonConfig) -> Option<UnixStream> {
    UnixStream::connect(socket_path(config)).ok()
}

/// The socket's directory must be ours and closed to others, or another
/// user could have put something there first.
fn prepare_dir(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let meta = std::fs::metadata(dir)?;
    // SAFETY: geteuid never fails.
    let me = unsafe { libc::geteuid() };
    if meta.uid() != me || meta.permissions().mode() & 0o077 != 0 {
        bail!(
            "{} must belong to you and be closed to others (chmod 700)",
            dir.display()
        );
    }
    Ok(())
}

/// The listening socket passed by systemd socket activation, if any.
pub(crate) fn systemd_listener() -> Option<UnixListener> {
    let pid: u32 = std::env::var("LISTEN_PID").ok()?.parse().ok()?;
    let fds: i32 = std::env::var("LISTEN_FDS").ok()?.parse().ok()?;
    if pid != std::process::id() || fds < 1 {
        return None;
    }
    // SAFETY: systemd passes the first socket as fd 3.
    Some(unsafe { UnixListener::from_raw_fd(3) })
}

/// The connecting process's (pid, uid).
fn peer(stream: &UnixStream) -> Option<(i32, u32)> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: a valid ucred buffer and length.
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (r == 0).then_some((cred.pid, cred.uid))
}

#[derive(Default)]
struct Stats {
    active: AtomicU64,
    total: AtomicU64,
    calls: AtomicU64,
    reloads: AtomicU64,
    reload_error: Mutex<Option<String>>,
}

/// Run until the process is stopped.
pub fn run(policy: Policy, source: Option<PathBuf>, socket: Option<PathBuf>) -> Result<()> {
    let path = socket.unwrap_or_else(|| socket_path(&policy.daemon));
    let listener = match systemd_listener() {
        Some(l) => l,
        None => {
            if let Some(dir) = path.parent() {
                prepare_dir(dir)?;
            }
            if path.exists() {
                if UnixStream::connect(&path).is_ok() {
                    bail!("an Orion daemon is already running on {}", path.display());
                }
                std::fs::remove_file(&path)?;
            }
            let l = UnixListener::bind(&path)
                .with_context(|| format!("listening on {}", path.display()))?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            l
        }
    };

    let watched = source.clone().or_else(Policy::default_config_path);
    let sandbox = crate::sandbox::describe(&policy);
    let offer_ask = policy.daemon.approvals;
    let broker = Arc::new(Broker::new(Duration::from_secs(policy.daemon.approval_timeout_secs.clamp(5, 3600))));
    let shared: Shared = Arc::new(RwLock::new(Arc::new(Orion::new(policy))));
    let stats = Arc::new(Stats::default());
    let started = Instant::now();
    let health: crate::watch::Shared = Arc::default();
    {
        let (shared, health) = (shared.clone(), health.clone());
        thread::spawn(move || crate::watch::run(move || shared.read().unwrap_or_else(|e| e.into_inner()).clone(), health));
    }
    tracing::info!(socket = %path.display(), policy = %describe_source(&source), sandbox = %sandbox, "orion daemon ready");

    if let Some(file) = watched.clone() {
        let (shared, stats) = (shared.clone(), stats.clone());
        thread::spawn(move || watch_policy(&file, &shared, &stats));
    }

    let status: Extension = {
        let (shared, stats, path, watched, broker, health) =
            (shared.clone(), stats.clone(), path.clone(), watched.clone(), broker.clone(), health.clone());
        Arc::new(move |method: &str, _params: &Value| {
            if method == "orion/health" {
                return Some(json!(health.lock().unwrap_or_else(|e| e.into_inner()).clone()));
            }
            (method == "orion/status").then(|| {
                let orion = shared.read().unwrap_or_else(|e| e.into_inner()).clone();
                let count = |d: Decision| orion.tools().filter(|(_, x)| *x == d).count();
                json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "pid": std::process::id(),
                    "uptime_secs": started.elapsed().as_secs(),
                    "socket": path,
                    "policy": watched.as_ref().filter(|p| p.exists()).map_or("built-in defaults".to_string(), |p| p.display().to_string()),
                    "reloads": stats.reloads.load(Ordering::Relaxed),
                    "reload_error": *stats.reload_error.lock().unwrap_or_else(|e| e.into_inner()),
                    "connections": { "active": stats.active.load(Ordering::Relaxed), "total": stats.total.load(Ordering::Relaxed) },
                    "tool_calls": stats.calls.load(Ordering::Relaxed),
                    "tools": { "allow": count(Decision::Allow), "ask": count(Decision::Ask), "deny": count(Decision::Deny) },
                    "sandbox": crate::sandbox::describe(orion.policy()),
                    "audit_log": orion.policy().audit_path(),
                    "approvals": { "offered": offer_ask, "approvers": broker.watching(), "waiting": broker.list().len() },
                    "health": health.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                })
            })
        })
    };

    // SAFETY: geteuid never fails.
    let me = unsafe { libc::geteuid() };
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        // The socket file is private already; check the peer anyway, in case
        // it was passed in by a socket activator with looser permissions.
        let Some((pid, uid)) = peer(&stream) else {
            continue;
        };
        if uid != me && uid != 0 {
            tracing::warn!(pid, uid, "refused a connection from another user");
            continue;
        }
        let (shared, stats, status, broker) = (shared.clone(), stats.clone(), status.clone(), broker.clone());
        thread::spawn(move || {
            stats.active.fetch_add(1, Ordering::Relaxed);
            let conn = stats.total.fetch_add(1, Ordering::Relaxed) + 1;
            let server = Arc::new(Server::shared(shared, format!("pid {pid}"), Some(status)).offering_ask(offer_ask));
            if let Err(e) = serve(&server, &stream, &stats, &broker, conn, offer_ask) {
                tracing::debug!(pid, "connection ended: {e:#}");
            }
            broker.unwatch(conn);
            stats.active.fetch_sub(1, Ordering::Relaxed);
        });
    }
    Ok(())
}

/// One connection: an MCP session, plus the daemon's own approval methods.
/// Tool calls run on threads of their own, so the connection keeps reading
/// while one waits: for an approval its own client gives (a chat), or for a
/// cancellation (`notifications/cancelled`, from Ctrl-C).
fn serve(server: &Arc<Server>, stream: &UnixStream, stats: &Arc<Stats>, broker: &Arc<Broker>, conn: u64, offer_ask: bool) -> Result<()> {
    let reader = BufReader::new(stream.try_clone()?);
    let out: Writer = Arc::new(Mutex::new(stream.try_clone()?));
    // Calls running now, by request id: their stop flags.
    let running: Arc<Mutex<std::collections::HashMap<String, Arc<std::sync::atomic::AtomicBool>>>> = Arc::default();
    // This client answers for its own calls (a chat through the daemon).
    let mut answers_itself = false;
    let reply = |id: &Value, result: Result<Value, (i64, String)>| -> Value {
        match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
        }
    };
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                let _ = approvals::send(&out, &json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("parse error: {e}") } }));
                continue;
            }
        };
        let has_id = msg.get("id").is_some();
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let answer = match msg["method"].as_str() {
            Some("orion/approvals/watch") => Some(Ok(json!({ "pending": broker.watch(conn, out.clone()) }))),
            Some("orion/approvals/list") => Some(Ok(json!({ "pending": broker.list() }))),
            Some("orion/approvals/answer") => match (params["id"].as_u64(), params["approve"].as_bool()) {
                (Some(rid), Some(yes)) => Some(Ok(json!({ "answered": broker.answer(rid, yes) }))),
                _ => Some(Err((-32602, "give id and approve".to_string()))),
            },
            Some("orion/approvals/self") => {
                answers_itself = true;
                Some(Ok(json!({})))
            }
            Some("notifications/cancelled") => {
                let key = params["requestId"].to_string();
                if let Some(flag) = running.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
                    flag.store(true, Ordering::SeqCst);
                }
                None
            }
            Some("tools/call") if has_id => {
                stats.calls.fetch_add(1, Ordering::Relaxed);
                let (server, out, broker, running) = (server.clone(), out.clone(), broker.clone(), running.clone());
                // With approvals on, there is always an approver: if nobody
                // watches, it says so in the refusal.
                let who = if answers_itself { Some(Ask::Caller(out.clone())) } else { offer_ask.then_some(Ask::Watchers) };
                let key = id.to_string();
                let id = id.clone();
                thread::spawn(move || {
                    let flag = crate::interrupt::thread_flag::install();
                    running.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), flag);
                    let approver = who.map(|who| BrokerApprover { broker: &broker, who, caller: server.call_caller(&params), last: Mutex::new(None) });
                    let result = server.call_tool(&params, approver.as_ref().map(|a| a as &dyn crate::Approver));
                    running.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
                    let msg = match result {
                        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
                        Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
                    };
                    let _ = approvals::send(&out, &msg);
                });
                None
            }
            _ => {
                if let Some(r) = server.handle(msg) {
                    let _ = approvals::send(&out, &r);
                }
                None
            }
        };
        if let Some(result) = answer {
            if has_id {
                approvals::send(&out, &reply(&id, result))?;
            }
        }
    }
    // The client is gone: stop what it started.
    for flag in running.lock().unwrap_or_else(|e| e.into_inner()).values() {
        flag.store(true, Ordering::SeqCst);
    }
    Ok(())
}

fn describe_source(source: &Option<PathBuf>) -> String {
    source
        .as_ref()
        .map_or("built-in defaults".into(), |p| p.display().to_string())
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Reload the policy when its file changes. A file that fails to parse is
/// reported and the running policy kept.
fn watch_policy(path: &Path, shared: &Shared, stats: &Stats) {
    let mut last = mtime(path);
    loop {
        thread::sleep(Duration::from_secs(2));
        let now = mtime(path);
        if now == last {
            continue;
        }
        last = now;
        if now.is_none() {
            continue;
        }
        match Policy::load(Some(path)) {
            Ok((policy, _)) => {
                *shared.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(Orion::new(policy));
                stats.reloads.fetch_add(1, Ordering::Relaxed);
                *stats.reload_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
                tracing::info!(policy = %path.display(), "policy reloaded");
            }
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::error!(policy = %path.display(), "policy not reloaded, keeping the old one: {msg}");
                *stats.reload_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
            }
        }
    }
}

/// Connect stdin and stdout to the daemon until either side closes.
///
/// An agent may never answer approval requests, its own or others': the
/// `orion/approvals/*` methods are refused here, before the daemon sees
/// them. People approve over their own connections (`orion approvals`, the
/// shell), never through `orion serve`.
pub fn bridge(stream: UnixStream) -> Result<()> {
    let mut to_daemon = stream.try_clone()?;
    let stdout = Arc::new(Mutex::new(io::stdout()));
    {
        let stdout = stdout.clone();
        thread::spawn(move || {
            let _ = forward_requests(&mut io::stdin().lock(), &mut to_daemon, &stdout);
            let _ = to_daemon.shutdown(Shutdown::Write);
        });
    }
    // Daemon to stdout: whole lines, so refusals can't land mid-message.
    for line in BufReader::new(&stream).lines() {
        let line = line?;
        let mut out = stdout.lock().unwrap_or_else(|e| e.into_inner());
        writeln!(out, "{line}")?;
        out.flush()?;
    }
    Ok(())
}

/// The agent's messages, line by line, to the daemon, except the methods
/// only a person's own connection may use, which get an error instead.
fn forward_requests(from: &mut impl BufRead, to: &mut impl Write, stdout: &Mutex<io::Stdout>) -> io::Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        if from.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let msg: Option<Value> = serde_json::from_str(line.trim()).ok();
        let method = msg.as_ref().and_then(|m| m["method"].as_str()).unwrap_or_default();
        if method.starts_with("orion/approvals") {
            if let Some(id) = msg.as_ref().and_then(|m| m.get("id")) {
                let refusal = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "an agent can't answer approval requests; a person does, with orion approvals or the shell" } });
                let mut out = stdout.lock().unwrap_or_else(|e| e.into_inner());
                writeln!(out, "{refusal}")?;
                out.flush()?;
            }
            continue;
        }
        to.write_all(line.as_bytes())?;
        if !line.ends_with('\n') {
            to.write_all(b"\n")?;
        }
        to.flush()?;
    }
}

/// Ask a running daemon for its status.
pub fn status(config: &DaemonConfig) -> Result<Value> {
    let path = socket_path(config);
    let stream = UnixStream::connect(&path)
        .with_context(|| format!("no Orion daemon on {}", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut w = &stream;
    writeln!(
        w,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","clientInfo":{{"name":"orion status","version":"{}"}}}}}}"#,
        env!("CARGO_PKG_VERSION")
    )?;
    writeln!(w, r#"{{"jsonrpc":"2.0","id":2,"method":"orion/status"}}"#)?;
    for line in BufReader::new(&stream).lines() {
        let v: Value = serde_json::from_str(&line?)?;
        if v["id"] == 2 {
            return Ok(v["result"].clone());
        }
    }
    bail!("the daemon closed the connection")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpc(stream: &UnixStream, id: u64, method: &str, params: Value) -> Value {
        let mut w = stream;
        writeln!(
            w,
            "{}",
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn serves_many_clients_with_one_policy_and_reloads_it() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("orion.toml");
        let audit = dir.path().join("audit.jsonl");
        let socket = dir.path().join("run/orion.sock");
        let write_policy = |extra: &str| {
            std::fs::write(
                &config,
                format!(
                    "[audit]\npath = {:?}\n[sandbox]\nmode = \"off\"\n{extra}",
                    audit.to_str().unwrap()
                ),
            )
            .unwrap();
        };
        write_policy("");
        let (policy, source) = Policy::load(Some(&config)).unwrap();
        let sock = socket.clone();
        thread::spawn(move || run(policy, source, Some(sock)));
        let start = Instant::now();
        while UnixStream::connect(&socket).is_err() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "daemon did not start"
            );
            thread::sleep(Duration::from_millis(20));
        }
        let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "only this user can connect");
        assert_eq!(
            std::fs::metadata(socket.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        // Two agents at once, each named in the shared audit log.
        let a = UnixStream::connect(&socket).unwrap();
        let b = UnixStream::connect(&socket).unwrap();
        rpc(
            &a,
            1,
            "initialize",
            json!({ "clientInfo": { "name": "agent-a" } }),
        );
        rpc(
            &b,
            1,
            "initialize",
            json!({ "clientInfo": { "name": "agent-b" } }),
        );
        assert_eq!(
            rpc(
                &a,
                2,
                "tools/call",
                json!({ "name": "system_info", "arguments": {} })
            )["result"]["isError"],
            false
        );
        assert_eq!(
            rpc(
                &b,
                2,
                "tools/call",
                json!({ "name": "system_info", "arguments": {} })
            )["result"]["isError"],
            false
        );
        let log = std::fs::read_to_string(&audit).unwrap();
        assert!(
            log.contains("\"caller\":\"agent-a (pid ")
                && log.contains("\"caller\":\"agent-b (pid "),
            "{log}"
        );

        let st = rpc(&a, 3, "orion/status", json!({}));
        assert_eq!(st["result"]["connections"]["active"], 2);
        assert_eq!(st["result"]["tool_calls"], 2);
        assert_eq!(st["result"]["sandbox"], "off: programs run unconfined");

        // Edit the policy: denying system_info takes effect on open connections.
        thread::sleep(Duration::from_millis(1100)); // a distinct mtime
        write_policy("[tools]\nsystem_info = \"deny\"\n");
        let start = Instant::now();
        loop {
            let r = rpc(
                &a,
                4,
                "tools/call",
                json!({ "name": "system_info", "arguments": {} }),
            );
            if r["result"]["isError"] == true {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(8),
                "policy was not reloaded"
            );
            thread::sleep(Duration::from_millis(200));
        }
        // A broken edit is reported and the running policy kept.
        thread::sleep(Duration::from_millis(1100));
        std::fs::write(&config, "[tools\n").unwrap();
        let start = Instant::now();
        loop {
            let st = rpc(&b, 5, "orion/status", json!({}));
            if st["result"]["reload_error"].is_string() {
                assert_eq!(st["result"]["reloads"], 1);
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(8),
                "bad policy not reported"
            );
            thread::sleep(Duration::from_millis(200));
        }
        assert_eq!(
            rpc(
                &b,
                6,
                "tools/call",
                json!({ "name": "system_info", "arguments": {} })
            )["result"]["isError"],
            true
        );
    }

    #[test]
    fn refuses_an_open_socket_directory() {
        let dir = tempfile::tempdir().unwrap();
        let open = dir.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(prepare_dir(&open)
            .unwrap_err()
            .to_string()
            .contains("closed to others"));
        assert!(prepare_dir(&dir.path().join("fresh")).is_ok());
    }
}
