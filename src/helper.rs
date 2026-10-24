//! The privileged helper (`orion helper`): a small root service that does
//! the root work of Orion's machine tools, and only that.
//!
//! Without it, Orion gets root with `sudo -n` or `pkexec`, and a sudo rule
//! for a package manager or `systemctl` lets anything running as you become
//! root. The helper narrows that to what Orion actually does. It accepts a
//! command on a Unix socket only from root, members of `[helper] group` and
//! the `users` listed, and runs it only if it is one Orion's tools build,
//! checked again on the helper's side against its own policy (a file only
//! root may change):
//!
//! - package manager commands, on valid package names, whose dry run (as
//!   root) removes no protected or essential package;
//! - `systemctl` actions on units that aren't protected;
//! - GPU pins (`50-orion-gpu.conf` drop-ins) holding only GPU variables;
//! - mounts under the mount roots, always `nosuid,nodev`, of disks the
//!   system doesn't run from, and unmounts of anything but the system's;
//! - NetworkManager's Wi-Fi and connection commands, and firewall rules
//!   that keep SSH open;
//! - snapper snapshots on the helper's own configuration;
//! - reading drives' SMART data;
//! - keeping checkpoints of each user's audit log (`ANCHOR`), which runs
//!   no program: it only appends a line to that user's file;
//! - the time zone, network time, locale and keyboard layout, with values
//!   the machine has.
//!
//! Each command runs with a clean environment and a fixed `PATH`, is
//! logged with who asked, and is stopped if the asker hangs up.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};

use crate::policy::Policy;
use crate::tools::exec::{self, Finished};

/// `[helper]`: where the helper listens and who may use it. Orion reads
/// `socket` from your policy; the helper reads all of it from its own.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HelperConfig {
    pub socket: String,
    /// Members of this group may use the helper.
    pub group: String,
    /// And these login names.
    pub users: Vec<String>,
    /// Where the helper keeps each user's audit-log checkpoints
    /// (`UID.jsonl`, root's, readable by all).
    pub anchors: String,
}

impl Default for HelperConfig {
    fn default() -> Self {
        Self {
            socket: "/run/orion/helper.sock".into(),
            group: "orion".into(),
            users: Vec::new(),
            anchors: "/var/lib/orion/audit-anchors".into(),
        }
    }
}

/// The request that checkpoints the asker's audit log: `[lines, sha256]`.
/// The helper handles it itself; no program runs.
pub const ANCHOR: &str = "orion-audit-anchor";
/// Checkpoints at most this often per user, and this many bytes of them.
const ANCHOR_EVERY_SECS: u64 = 60;
const ANCHOR_MAX_BYTES: u64 = 8 << 20;

/// Append a checkpoint for `uid` to `dir/UID.jsonl`.
fn anchor(dir: &Path, uid: u32, args: &[String]) -> Result<()> {
    let [lines, hash] = args else { bail!("a checkpoint is a line count and a hash") };
    let lines: u64 = lines.parse().context("the line count isn't a number")?;
    ensure!(hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()), "that isn't a SHA-256 hash");
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
    let path = dir.join(format!("{uid}.jsonl"));
    if let Ok(meta) = std::fs::symlink_metadata(&path) {
        ensure!(meta.file_type().is_file() && meta.uid() == 0, "{} isn't the helper's own file", path.display());
        ensure!(meta.len() < ANCHOR_MAX_BYTES, "uid {uid} has checkpointed too much already");
        let age = meta.modified().ok().and_then(|m| m.elapsed().ok()).unwrap_or(Duration::MAX);
        ensure!(age >= Duration::from_secs(ANCHOR_EVERY_SECS), "one checkpoint a minute at most");
    }
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let line = serde_json::json!({ "at": crate::tools::journal_time(secs), "uid": uid, "lines": lines, "hash": hash });
    let mut f = std::fs::OpenOptions::new().create(true).append(true).mode(0o644).open(&path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

/// The helper's own policy file.
pub const CONFIG: &str = "/etc/orion/helper.toml";
/// Where the helper finds programs: never a directory a user could write.
const SAFE_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";
const MAX_TIMEOUT_SECS: u64 = 3600;
const MAX_REQUEST: u64 = 1 << 20;

#[derive(Serialize, Deserialize)]
struct Request {
    program: String,
    args: Vec<String>,
    stdin: Option<String>,
    timeout_secs: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct Response {
    /// Why the helper wouldn't run it; nothing ran.
    refused: Option<String>,
    /// The raw wait status.
    status: i32,
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
    timed_out: bool,
    duration_ms: u64,
}

/// Who is asking, from the socket's peer credentials.
#[derive(Debug, Clone, Copy)]
pub struct Peer {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

/// A command a check let through: its arguments (a check may rewrite them,
/// as for a password file), its input, and what must live while it runs.
pub struct Checked {
    pub args: Vec<String>,
    pub stdin: Option<String>,
    pub keep: Vec<Box<dyn std::any::Any + Send>>,
}

impl Checked {
    pub fn as_is(args: &[String], stdin: Option<&str>) -> Self {
        Self { args: args.to_vec(), stdin: stdin.map(String::from), keep: Vec::new() }
    }
}

/// Run `program args…` through the helper at `socket`, as Orion would run
/// it with sudo. Ctrl-C hangs up, which stops the command.
pub fn run(socket: &Path, program: &str, args: &[String], stdin: Option<String>, timeout: Duration) -> Result<Finished> {
    let mut stream = UnixStream::connect(socket).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => anyhow::anyhow!(
            "the Orion helper isn't running at {} (sudo systemctl enable --now orion-helper.socket), \
             or set [admin] elevate to another way",
            socket.display()
        ),
        std::io::ErrorKind::PermissionDenied => anyhow::anyhow!(
            "you may not use the Orion helper at {}: join its group (sudo usermod -aG orion $USER, \
             then log in again)",
            socket.display()
        ),
        _ => anyhow::anyhow!("connecting to the Orion helper at {}: {e}", socket.display()),
    })?;
    let req = Request { program: program.into(), args: args.to_vec(), stdin, timeout_secs: timeout.as_secs().max(1) };
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).context("sending to the Orion helper")?;
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    let mut reply = Vec::new();
    let mut buf = [0u8; 64 << 10];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                reply.extend_from_slice(&buf[..n]);
                if reply.ends_with(b"\n") {
                    break;
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                if crate::interrupt::requested() {
                    // Hanging up stops the command on the helper's side.
                    return Err(crate::interrupt::Interrupted.into());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e).context("reading from the Orion helper"),
        }
    }
    ensure!(!reply.is_empty(), "the Orion helper hung up without an answer (see journalctl -u orion-helper)");
    let r: Response = serde_json::from_slice(&reply).context("the Orion helper's answer")?;
    if let Some(why) = r.refused {
        bail!("the Orion helper refused: {why}");
    }
    Ok(Finished {
        status: ExitStatus::from_raw(r.status),
        stdout: r.stdout,
        stderr: r.stderr,
        stdout_truncated: r.stdout_truncated,
        stderr_truncated: r.stderr_truncated,
        timed_out: r.timed_out,
        duration: Duration::from_millis(r.duration_ms),
    })
}

/// `orion helper`: load the helper's policy and serve until stopped.
pub fn main(config: Option<&Path>, socket: Option<&Path>) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    ensure!(unsafe { libc::geteuid() } == 0, "the Orion helper runs as root (see contrib/orion-helper.service)");
    let path = config.unwrap_or(Path::new(CONFIG));
    let mut policy = if path.exists() {
        // Its protected lists are the point: only root may change them, nor
        // swap the file for another.
        for p in [path, path.parent().unwrap_or(Path::new("/"))] {
            let meta = std::fs::metadata(p)?;
            ensure!(
                meta.uid() == 0 && meta.permissions().mode() & 0o022 == 0,
                "{} must belong to root and be writable only by root",
                p.display()
            );
        }
        Policy::load(Some(path))?.0
    } else {
        Policy::from_toml("")?
    };
    // The helper mounts only under the mount roots, never in a home folder.
    policy.fs.write_roots.clear();
    // It is root: whatever it runs, it runs directly.
    policy.admin.elevate = "none".into();
    let socket = socket.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(&policy.helper.socket));
    serve(Arc::new(policy), &socket)
}

fn serve(policy: Arc<Policy>, socket: &Path) -> Result<()> {
    let listener = match crate::daemon::systemd_listener() {
        Some(l) => l,
        None => bind(socket, &policy.helper)?,
    };
    tracing::info!(
        "Orion helper listening on {} for root, group {:?}{}",
        socket.display(),
        policy.helper.group,
        if policy.helper.users.is_empty() { String::new() } else { format!(" and {}", policy.helper.users.join(", ")) }
    );
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let policy = Arc::clone(&policy);
        std::thread::spawn(move || {
            if let Err(e) = handle(&policy, stream) {
                tracing::warn!("{e:#}");
            }
        });
    }
    Ok(())
}

/// Listen at `path`: root's, and only the helper's group may connect.
fn bind(path: &Path, cfg: &HelperConfig) -> Result<UnixListener> {
    let gid = group_gid(&cfg.group)
        .with_context(|| format!("the group {:?} doesn't exist; create it with: groupadd --system {}", cfg.group, cfg.group))?;
    let dir = path.parent().context("the socket needs a directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let meta = std::fs::metadata(dir)?;
    ensure!(
        meta.uid() == 0 && meta.permissions().mode() & 0o022 == 0,
        "{} must belong to root and be writable only by root",
        dir.display()
    );
    if std::fs::symlink_metadata(path).is_ok() {
        std::fs::remove_file(path).with_context(|| format!("removing the old {}", path.display()))?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("listening on {}", path.display()))?;
    std::os::unix::fs::chown(path, Some(0), Some(gid))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    Ok(listener)
}

fn handle(policy: &Policy, stream: UnixStream) -> Result<()> {
    let peer = peer(&stream).context("reading who connected")?;
    let who = format!("uid {} (pid {})", peer.uid, peer.pid);
    let answer = |r: &Response| -> Result<()> {
        let mut line = serde_json::to_string(r)?;
        line.push('\n');
        stream.try_clone()?.write_all(line.as_bytes())?;
        Ok(())
    };
    if !allowed(&policy.helper, &peer) {
        tracing::warn!("{who}: refused: not root, in group {:?} or in users", policy.helper.group);
        return answer(&Response { refused: Some(format!("uid {} may not use the Orion helper", peer.uid)), ..Default::default() });
    }
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut line = String::new();
    BufReader::new(stream.try_clone()?.take(MAX_REQUEST)).read_line(&mut line)?;
    let req: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => return answer(&Response { refused: Some(format!("not a request: {e}")), ..Default::default() }),
    };
    if req.program == ANCHOR {
        return match anchor(Path::new(&policy.helper.anchors), peer.uid, &req.args) {
            Ok(()) => {
                tracing::info!("{who}: audit checkpoint at entry {}", req.args.first().map(String::as_str).unwrap_or("?"));
                answer(&Response { stdout: "checkpointed\n".into(), ..Default::default() })
            }
            Err(e) => answer(&Response { refused: Some(format!("{e:#}")), ..Default::default() }),
        };
    }
    let checked = match check(policy, &peer, &req) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("{who}: refused {} {:?}: {e:#}", req.program, req.args);
            return answer(&Response { refused: Some(format!("{e:#}")), ..Default::default() });
        }
    };
    tracing::info!("{who}: {} {}", req.program, checked.args.join(" "));
    let mut cmd = Command::new(program_path(&req.program)?);
    cmd.args(&checked.args)
        .env_clear()
        .env("PATH", SAFE_PATH)
        .env("HOME", "/root")
        .env("LC_ALL", "C")
        .env("SYSTEMD_COLORS", "0")
        .env("SYSTEMD_PAGER", "");
    let fd = stream.as_raw_fd();
    let timeout = Duration::from_secs(req.timeout_secs.clamp(1, MAX_TIMEOUT_SECS));
    // A package manager stopped halfway can leave the package database
    // broken, so it finishes even if the asker hangs up; the rest stop.
    let finish = matches!(req.program.as_str(), "env" | "dnf" | "zypper" | "pacman");
    let done = exec::run_until(cmd, checked.stdin, timeout, 4 << 20, || !finish && hung_up(fd));
    drop(checked.keep);
    match done {
        Ok(d) => {
            tracing::info!("{who}: {} exited {}", req.program, d.status);
            answer(&Response {
                refused: None,
                status: d.status.into_raw(),
                stdout: d.stdout,
                stderr: d.stderr,
                stdout_truncated: d.stdout_truncated,
                stderr_truncated: d.stderr_truncated,
                timed_out: d.timed_out,
                duration_ms: d.duration.as_millis() as u64,
            })
        }
        Err(e) if e.is::<crate::interrupt::Interrupted>() => {
            tracing::info!("{who}: hung up; stopped {}", req.program);
            Ok(())
        }
        Err(e) => answer(&Response { refused: Some(format!("{e:#}")), ..Default::default() }),
    }
}

/// The checks every request passes, then the owning tool's own check.
fn check(policy: &Policy, peer: &Peer, req: &Request) -> Result<Checked> {
    ensure!(req.args.len() <= 400, "too many arguments");
    let sizes_ok = req.args.iter().all(|a| a.len() <= 4096 && !a.contains('\0'));
    ensure!(sizes_ok, "an argument is too long or holds a NUL byte");
    ensure!(req.stdin.as_ref().is_none_or(|s| s.len() <= 64 << 10), "the input is too long");
    crate::tools::helper_check(policy, peer, &req.program, &req.args, req.stdin.as_deref())
}

/// The full path of a program, from the safe directories only.
fn program_path(name: &str) -> Result<PathBuf> {
    ensure!(!name.contains('/'), "a program name, not a path");
    SAFE_PATH
        .split(':')
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
        .with_context(|| format!("{name} isn't installed"))
}

/// Whether whoever asked has hung up (or Ctrl-C'd).
fn hung_up(fd: i32) -> bool {
    let mut p = libc::pollfd { fd, events: libc::POLLRDHUP, revents: 0 };
    // SAFETY: one valid pollfd, no waiting.
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    n > 0 && p.revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR) != 0
}

fn peer(stream: &UnixStream) -> Option<Peer> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: a valid ucred buffer and length.
    let r = unsafe {
        libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len)
    };
    (r == 0).then_some(Peer { pid: cred.pid, uid: cred.uid, gid: cred.gid })
}

/// Root, a listed user, or a member of the group.
fn allowed(cfg: &HelperConfig, peer: &Peer) -> bool {
    if peer.uid == 0 {
        return true;
    }
    let Some(name) = user_name(peer.uid) else { return false };
    if cfg.users.contains(&name) {
        return true;
    }
    let Some(gid) = group_gid(&cfg.group) else { return false };
    peer.gid == gid || user_groups(&name, peer.gid).contains(&gid)
}

fn user_name(uid: u32) -> Option<String> {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16 << 10];
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: valid buffers; `out` is set only on success.
    let r = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut out) };
    if r != 0 || out.is_null() {
        return None;
    }
    // SAFETY: getpwuid_r succeeded, so pw_name points into `buf`.
    Some(unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) }.to_string_lossy().into_owned())
}

fn group_gid(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 64 << 10];
    let mut out: *mut libc::group = std::ptr::null_mut();
    // SAFETY: valid buffers; `out` is set only on success.
    let r = unsafe { libc::getgrnam_r(c.as_ptr(), &mut grp, buf.as_mut_ptr(), buf.len(), &mut out) };
    (r == 0 && !out.is_null()).then_some(grp.gr_gid)
}

fn user_groups(name: &str, primary: u32) -> Vec<u32> {
    let Ok(c) = std::ffi::CString::new(name) else { return Vec::new() };
    let mut n: libc::c_int = 64;
    loop {
        let mut groups = vec![0 as libc::gid_t; n as usize];
        let want = n;
        // SAFETY: `groups` holds `n` entries; getgrouplist writes at most that.
        let r = unsafe { libc::getgrouplist(c.as_ptr(), primary, groups.as_mut_ptr(), &mut n) };
        if r >= 0 {
            groups.truncate(n as usize);
            return groups;
        }
        if n <= want {
            n = want * 2;
        }
        if n > 65536 {
            return Vec::new();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_users_and_groups() {
        assert_eq!(user_name(0).as_deref(), Some("root"));
        assert_eq!(group_gid("root"), Some(0));
        assert!(group_gid("no-such-group-orion").is_none());
        assert!(user_groups("root", 0).contains(&0));
        let cfg = HelperConfig { group: "root".into(), ..Default::default() };
        assert!(allowed(&cfg, &Peer { pid: 1, uid: 0, gid: 0 }));
        let nobody = Peer { pid: 1, uid: 65534, gid: 65534 };
        assert!(!allowed(&cfg, &nobody));
        let listed = HelperConfig { users: vec![user_name(65534).unwrap_or_default()], ..Default::default() };
        assert!(allowed(&listed, &nobody));
    }

    #[test]
    fn keeps_checkpoints_carefully() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("anchors");
        let h = "a".repeat(64);
        // Not root here, so the file is "not the helper's own" on a second write;
        // the checks before writing are what this tests.
        assert!(anchor(&d, 1000, &["12".into()]).is_err());
        assert!(anchor(&d, 1000, &["x".into(), h.clone()]).is_err());
        assert!(anchor(&d, 1000, &["12".into(), "A".repeat(64)]).is_err());
        assert!(anchor(&d, 1000, &["12".into(), "../x".into()]).is_err());
        anchor(&d, 1000, &["12".into(), h.clone()]).unwrap();
        let text = std::fs::read_to_string(d.join("1000.jsonl")).unwrap();
        assert!(text.contains("\"lines\":12") && text.contains(&h));
        // At most one a minute (or, unprivileged, the file isn't root's).
        assert!(anchor(&d, 1000, &["13".into(), h]).is_err());
    }

    #[test]
    fn programs_come_from_safe_directories() {
        assert!(program_path("sh").is_ok());
        assert!(program_path("../bin/sh").is_err());
        assert!(program_path("/bin/sh").is_err());
        assert!(program_path("orion-no-such-program").is_err());
    }
}
