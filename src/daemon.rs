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

use crate::mcp::{Extension, Server, Shared};
use crate::policy::expand_home;
use crate::{Decision, Orion, Policy};

/// `[daemon]` in orion.toml.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// Empty: `$XDG_RUNTIME_DIR/orion/orion.sock`.
    pub socket: String,
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
fn systemd_listener() -> Option<UnixListener> {
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
    let shared: Shared = Arc::new(RwLock::new(Arc::new(Orion::new(policy))));
    let stats = Arc::new(Stats::default());
    let started = Instant::now();
    tracing::info!(socket = %path.display(), policy = %describe_source(&source), sandbox = %sandbox, "orion daemon ready");

    if let Some(file) = watched.clone() {
        let (shared, stats) = (shared.clone(), stats.clone());
        thread::spawn(move || watch_policy(&file, &shared, &stats));
    }

    let status: Extension = {
        let (shared, stats, path, watched) =
            (shared.clone(), stats.clone(), path.clone(), watched.clone());
        Arc::new(move |method: &str, _params: &Value| {
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
        let (shared, stats, status) = (shared.clone(), stats.clone(), status.clone());
        thread::spawn(move || {
            stats.active.fetch_add(1, Ordering::Relaxed);
            stats.total.fetch_add(1, Ordering::Relaxed);
            let server = Server::shared(shared, format!("pid {pid}"), Some(status));
            if let Err(e) = serve(&server, &stream, &stats) {
                tracing::debug!(pid, "connection ended: {e:#}");
            }
            stats.active.fetch_sub(1, Ordering::Relaxed);
        });
    }
    Ok(())
}

/// One MCP session, counting tool calls.
fn serve(server: &Server, stream: &UnixStream, stats: &Stats) -> Result<()> {
    let reader = BufReader::new(stream.try_clone()?);
    let mut out = stream;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => {
                if msg["method"] == "tools/call" {
                    stats.calls.fetch_add(1, Ordering::Relaxed);
                }
                server.handle(msg)
            }
            Err(e) => Some(
                json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("parse error: {e}") } }),
            ),
        };
        if let Some(reply) = reply {
            writeln!(out, "{reply}")?;
            out.flush()?;
        }
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
pub fn bridge(stream: UnixStream) -> Result<()> {
    let mut to_daemon = stream.try_clone()?;
    thread::spawn(move || {
        let _ = pump(&mut io::stdin(), &mut to_daemon);
        let _ = to_daemon.shutdown(Shutdown::Write);
    });
    pump(&mut &stream, &mut io::stdout())?;
    Ok(())
}

/// Copy until EOF, passing on each read at once. (Not `io::copy`: on Linux
/// it may splice pipe to socket in the kernel and hold messages back.)
fn pump(from: &mut impl io::Read, to: &mut impl Write) -> io::Result<()> {
    let mut buf = [0u8; 16 << 10];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        to.write_all(&buf[..n])?;
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
