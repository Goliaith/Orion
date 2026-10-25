//! Root work beyond the machine tools: `root_run` runs any program as root,
//! and `sys_edit` writes a file outside the writable roots (a file in
//! `/etc`, say), as root.
//!
//! Both reach further than anything else Orion offers, so:
//!
//! - a person approves each call, whatever the policy says (`ALWAYS_ASK`),
//!   and none can run on a timer or with nobody there;
//! - they go only through the privileged helper, which runs them only for
//!   the logins in its own `[helper] root_users`, a list only root may
//!   change, and logs each with who asked;
//! - each is snapshotted with snapper, before and after, and written in the
//!   change history, so `pkg_rollback` puts it back (`[root] snapshots`);
//! - they refuse to run while programs in `proc_run`'s sandbox could reach
//!   the helper's or the daemon's socket themselves (anything but
//!   bubblewrap), since such a program could then do root work, or answer
//!   its own approval, without the person.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{changes, exec};
use crate::helper::{Checked, HelperConfig, Peer};
use crate::policy::{Decision, Policy};
use crate::sandbox::Backend;
use crate::tool::{parse_args, Preapproval, Risk, Tool};

/// The helper request that runs a program as root: its arguments are the
/// program and its own arguments.
pub const RUN: &str = "orion-root-run";
/// The helper request that writes a file as root: the path, and for a new
/// file its mode; the content is the input.
pub const EDIT: &str = "orion-sys-edit";

/// The largest file `sys_edit` writes: what the helper takes as input.
const MAX_EDIT: usize = 64 << 10;

/// Where `sys_edit` never writes: what isn't a file (the kernel's and the
/// devices'), what snapper doesn't snapshot with the root filesystem (home
/// folders, the boot partition, temporary files), and what the helper
/// can't write anyway (it sees home folders read-only and a /tmp of its own).
const NO_EDIT: &[&str] = &["/proc", "/sys", "/dev", "/run", "/home", "/root", "/tmp", "/var/tmp", "/boot", "/efi", "/mnt", "/media"];

/// Whether `peer` may have the helper do root work for it: root, or a
/// login in `[helper] root_users`.
fn may_root(cfg: &HelperConfig, peer: &Peer) -> bool {
    peer.uid == 0 || crate::helper::user_name(peer.uid).is_some_and(|n| cfg.root_users.contains(&n))
}

fn refuse_unlisted(cfg: &HelperConfig, peer: &Peer) -> Result<()> {
    ensure!(
        may_root(cfg, peer),
        "root_run and sys_edit are off for uid {}: root must add the login to [helper] root_users in {}",
        peer.uid,
        crate::helper::CONFIG
    );
    Ok(())
}

/// The program `root_run` names, as the helper will run it: a name found
/// in the helper's safe directories, or an absolute path that only root
/// could have put there.
fn root_program(name: &str) -> Result<PathBuf> {
    if !name.contains('/') {
        return crate::helper::program_path(name);
    }
    let path = Path::new(name);
    ensure!(path.is_absolute(), "a program name, or an absolute path");
    let real = path.canonicalize().with_context(|| format!("{name} doesn't exist"))?;
    let meta = std::fs::metadata(&real)?;
    ensure!(meta.is_file() && meta.permissions().mode() & 0o111 != 0, "{} isn't a program", real.display());
    for p in real.ancestors() {
        let m = std::fs::metadata(p)?;
        ensure!(
            m.uid() == 0 && m.permissions().mode() & 0o022 == 0,
            "{} isn't root's alone, so someone else could have changed what runs; use a program root installed",
            p.display()
        );
    }
    Ok(real)
}

/// The privileged helper's check of a `root_run` request.
pub(crate) fn helper_check_run(policy: &Policy, peer: &Peer, args: &[String], stdin: Option<&str>) -> Result<Checked> {
    refuse_unlisted(&policy.helper, peer)?;
    let (program, rest) = args.split_first().context("no program to run")?;
    let path = root_program(program)?;
    Ok(Checked { program: Some(path), ..Checked::as_is(rest, stdin) })
}

/// Where `sys_edit` would write `raw`, and whether the file is there
/// already. The folder must exist and be root's alone, and the file, if
/// there, a plain file: not a link (name what it points to instead).
pub(crate) fn edit_target(raw: &str) -> Result<(PathBuf, bool)> {
    let path = Path::new(raw);
    ensure!(path.is_absolute(), "the path must be absolute: {raw}");
    let plain = raw.split('/').all(|c| c != "." && c != "..") && path.components().all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
    ensure!(plain, "no . or .. in the path: {raw}");
    let name = path.file_name().context("the path names no file")?;
    let parent = path.parent().context("the path names no file")?;
    let dir = parent.canonicalize().with_context(|| format!("{} doesn't exist (make the folder with root_run first)", parent.display()))?;
    let target = dir.join(name);
    if let Some(top) = NO_EDIT.iter().find(|p| target.starts_with(p)) {
        bail!("sys_edit doesn't write under {top}: nothing there is a plain file a snapshot can put back (fs_write writes in your own folders)");
    }
    let meta = std::fs::metadata(&dir)?;
    ensure!(meta.is_dir(), "{} isn't a folder", dir.display());
    ensure!(
        meta.uid() == 0 && meta.permissions().mode() & 0o022 == 0,
        "{} isn't root's alone; sys_edit writes only where only root can",
        dir.display()
    );
    match std::fs::symlink_metadata(&target) {
        Ok(m) if m.file_type().is_symlink() => {
            let to = target.canonicalize().map(|p| p.display().to_string()).unwrap_or_else(|_| "nowhere".into());
            bail!("{} is a link to {to}; edit that file instead", target.display())
        }
        Ok(m) if !m.is_file() => bail!("{} isn't a plain file", target.display()),
        Ok(_) => Ok((target, true)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((target, false)),
        Err(e) => Err(e).with_context(|| format!("{}", target.display())),
    }
}

/// A new file's mode: three or four octal digits, no set-id or sticky bits.
fn parse_mode(raw: &str) -> Result<u32> {
    let ok = (3..=4).contains(&raw.len()) && raw.chars().all(|c| ('0'..='7').contains(&c));
    let mode = if ok { u32::from_str_radix(raw, 8).ok() } else { None };
    match mode {
        Some(m) if m <= 0o777 => Ok(m),
        _ => bail!("mode is like \"0644\": read and write bits only, not {raw:?}"),
    }
}

/// The privileged helper's `sys_edit`: check the request and write the
/// file, in place when it exists (keeping its owner, mode and SELinux
/// label), as a new root-owned file otherwise.
pub(crate) fn helper_edit(policy: &Policy, peer: &Peer, args: &[String], content: Option<&str>) -> Result<String> {
    refuse_unlisted(&policy.helper, peer)?;
    let content = content.unwrap_or_default();
    ensure!(content.len() <= MAX_EDIT, "sys_edit writes files up to {} KiB", MAX_EDIT >> 10);
    let (raw, mode) = match args {
        [path] => (path, None),
        [path, mode] => (path, Some(parse_mode(mode)?)),
        _ => bail!("sys_edit takes a path, and a mode for a new file"),
    };
    let (path, exists) = edit_target(raw)?;
    ensure!(!(exists && mode.is_some()), "{} exists; a mode is only for a new file", path.display());
    let mut file = if exists {
        OpenOptions::new().write(true).truncate(true).custom_flags(libc::O_NOFOLLOW).open(&path)
    } else {
        OpenOptions::new().write(true).create_new(true).mode(mode.unwrap_or(0o644)).custom_flags(libc::O_NOFOLLOW).open(&path)
    }
    .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(content.as_bytes()).and_then(|()| file.sync_all()).with_context(|| format!("writing {}", path.display()))?;
    Ok(format!("{} {} ({} bytes)", if exists { "rewrote" } else { "created" }, path.display(), content.len()))
}

/// Where root work goes: the helper's socket, or nowhere when Orion is
/// root itself.
fn route(policy: &Policy) -> Result<Option<PathBuf>> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return Ok(None);
    }
    match exec::elevation(policy)? {
        Some(exec::Elevation::Helper(socket)) => Ok(Some(socket)),
        _ => bail!(
            "root_run and sys_edit go only through the Orion helper, which checks who may use them, and it isn't \
             running here (or [admin] elevate isn't auto or helper); see docs/install.md, \"The privileged helper\""
        ),
    }
}

/// Refuse while a sandboxed program could reach the helper or the daemon.
fn sandbox_closed(policy: &Policy) -> Result<()> {
    if policy.decide("proc_run", Risk::Execute) == Decision::Deny {
        return Ok(());
    }
    match crate::sandbox::backend(&policy.sandbox)? {
        Backend::Bubblewrap => Ok(()),
        b => bail!(
            "programs proc_run starts can reach the helper's and the daemon's sockets under the {} sandbox, so one \
             could do root work, or approve its own, without you. Install bubblewrap (sudo dnf install bubblewrap) \
             with [sandbox] mode auto or bubblewrap, or set [tools] proc_run = \"deny\"",
            b.name()
        ),
    }
}

/// The checks both tools make before anyone is asked.
fn precheck(policy: &Policy) -> Result<()> {
    route(policy)?;
    sandbox_closed(policy)?;
    changes::precheck_root(policy)
}

pub struct RootRun;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArgs {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    stdin: Option<String>,
    timeout_secs: Option<u64>,
}

impl Tool for RootRun {
    fn name(&self) -> &'static str {
        "root_run"
    }

    fn description(&self) -> &'static str {
        "Run one program as root, outside the sandbox, for work no other tool covers (sysctl, a repository's \
         setup, restorecon, a command a guide gives). No shell: pipes and redirects are not interpreted. Prefer \
         the dedicated tools (pkg_install, svc_control, fw_rule, …), which check more, and sys_edit for writing a \
         file. The person approves every call. A snapper snapshot is taken before and after, so pkg_rollback puts \
         it back; home folders are read-only to it and aren't in the snapshot."
    }

    fn risk(&self) -> Risk {
        Risk::Execute
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Program name (from /usr/sbin, /usr/bin, /sbin, /bin) or an absolute path root owns." },
                "args": { "type": "array", "items": { "type": "string" }, "default": [] },
                "stdin": { "type": "string", "description": "Text written to the program's standard input (up to 64 KiB)." },
                "timeout_secs": { "type": "integer", "minimum": 1, "description": "Capped by the policy's root.timeout_secs." }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    fn preapproval(&self, _policy: &Policy, _args: &Value) -> Preapproval {
        Preapproval::Person("it runs as root".into())
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        let a: RunArgs = parse_args(args.clone())?;
        ensure!(!a.command.is_empty(), "no program to run");
        ensure!(a.stdin.as_ref().is_none_or(|s| s.len() <= MAX_EDIT), "the input is too long (64 KiB at most)");
        precheck(policy)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: RunArgs = parse_args(args)?;
        let cap = policy.root.timeout_secs.max(1);
        let timeout = Duration::from_secs(a.timeout_secs.unwrap_or(cap).clamp(1, cap));
        let tracker = changes::begin_root(policy, "root_run")?;
        let done = match route(policy)? {
            Some(socket) => {
                let argv: Vec<String> = std::iter::once(a.command.clone()).chain(a.args).collect();
                crate::helper::run(&socket, RUN, &argv, a.stdin, timeout)
            }
            None => exec::run(exec::command_as(None, &root_program(&a.command)?.to_string_lossy(), &a.args), a.stdin, timeout, 4 << 20),
        };
        let mut out = match &done {
            Ok(d) => json!({
                "ok": d.success(),
                "exit_code": d.status.code(),
                "signal": d.signal(),
                "timed_out": d.timed_out,
                "duration_ms": d.duration.as_millis(),
                "stdout": d.stdout,
                "stderr": d.stderr,
                "stdout_truncated": d.stdout_truncated,
                "stderr_truncated": d.stderr_truncated,
            }),
            Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
        };
        let ok = done.as_ref().is_ok_and(exec::Finished::success);
        tracker.finish(policy, ok, &mut out);
        Ok(out)
    }
}

pub struct SysEdit;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    content: String,
    mode: Option<String>,
}

impl SysEdit {
    /// What the file holds now, if Orion can read it.
    fn current(path: &str) -> Option<String> {
        let (target, exists) = edit_target(path).ok()?;
        if !exists {
            return Some(String::new());
        }
        std::fs::read_to_string(target).ok()
    }
}

impl Tool for SysEdit {
    fn name(&self) -> &'static str {
        "sys_edit"
    }

    fn description(&self) -> &'static str {
        "Write a system file as root, outside the writable roots: a file in /etc, /usr/local or /opt, say. Give \
         the file's whole new content; read it first with fs_read. An existing file keeps its owner, mode and \
         SELinux label; a new one belongs to root, with mode (default 0644). The person approves every call, \
         seeing the change as a diff. A snapper snapshot is taken before and after, so pkg_rollback puts it back. \
         Not for home folders (fs_write), /boot, /proc, /sys, /dev or /run. Up to 64 KiB."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The file's absolute path; its folder must exist." },
                "content": { "type": "string", "description": "The file's whole new content." },
                "mode": { "type": "string", "description": "For a new file only: its mode, like \"0644\" or \"0600\"." }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    fn preapproval(&self, _policy: &Policy, _args: &Value) -> Preapproval {
        Preapproval::Person("it writes a system file as root".into())
    }

    /// The change as a diff against what the file holds now, rather than
    /// the whole new content, for the approval and the audit log.
    fn redact(&self, args: &Value) -> Value {
        let Ok(a) = parse_args::<EditArgs>(args.clone()) else { return args.clone() };
        let mut shown = json!({ "path": a.path, "bytes": a.content.len() });
        if let Some(mode) = &a.mode {
            shown["mode"] = json!(mode);
        }
        match Self::current(&a.path) {
            Some(old) => shown["diff"] = json!(diff(&old, &a.content)),
            // Root's alone to read (or not there to read): the whole content, then.
            None => {
                shown["content"] = json!(a.content);
                shown["note"] = json!("the file can't be read as you, so there is no diff");
            }
        }
        shown
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        let a: EditArgs = parse_args(args.clone())?;
        ensure!(a.content.len() <= MAX_EDIT, "sys_edit writes files up to {} KiB", MAX_EDIT >> 10);
        let (path, exists) = edit_target(&a.path)?;
        if let Some(mode) = &a.mode {
            parse_mode(mode)?;
            ensure!(!exists, "{} exists; mode is only for a new file", path.display());
        }
        if exists && std::fs::read_to_string(&path).is_ok_and(|now| now == a.content) {
            bail!("{} already holds exactly this", path.display());
        }
        precheck(policy)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: EditArgs = parse_args(args)?;
        let argv: Vec<String> = std::iter::once(a.path.clone()).chain(a.mode.clone()).collect();
        let tracker = changes::begin_root(policy, "sys_edit")?;
        let done = match route(policy)? {
            Some(socket) => crate::helper::run(&socket, EDIT, &argv, Some(a.content), Duration::from_secs(30)).and_then(|d| {
                ensure!(d.success(), "{}", d.complaint());
                Ok(d.stdout.trim().to_string())
            }),
            None => helper_edit(policy, &Peer { pid: std::process::id() as i32, uid: 0, gid: 0 }, &argv, Some(&a.content)),
        };
        let mut out = match &done {
            Ok(what) => json!({ "ok": true, "path": a.path, "message": what }),
            Err(e) => json!({ "ok": false, "path": a.path, "error": format!("{e:#}") }),
        };
        tracker.finish(policy, done.is_ok(), &mut out);
        Ok(out)
    }
}

/// The lines that changed from `old` to `new`, as a unified diff with two
/// lines of context, capped.
pub(crate) fn diff(old: &str, new: &str) -> String {
    #[derive(Clone, Copy, PartialEq)]
    enum Op {
        Same,
        Del,
        Add,
    }
    let (a, b): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let post = a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (ma, mb) = (&a[pre..a.len() - post], &b[pre..b.len() - post]);
    // The middle by longest common subsequence, when it is small enough;
    // otherwise all of it out and all of it in.
    let mut mid = Vec::new();
    if ma.len().saturating_mul(mb.len()) <= 1 << 20 {
        let (n, m) = (ma.len(), mb.len());
        let mut lcs = vec![0u32; (n + 1) * (m + 1)];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i * (m + 1) + j] =
                    if ma[i] == mb[j] { lcs[(i + 1) * (m + 1) + j + 1] + 1 } else { lcs[(i + 1) * (m + 1) + j].max(lcs[i * (m + 1) + j + 1]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && ma[i] == mb[j] {
                mid.push(Op::Same);
                (i, j) = (i + 1, j + 1);
            } else if i < n && (j == m || lcs[(i + 1) * (m + 1) + j] >= lcs[i * (m + 1) + j + 1]) {
                mid.push(Op::Del);
                i += 1;
            } else {
                mid.push(Op::Add);
                j += 1;
            }
        }
    } else {
        mid.extend(std::iter::repeat_n(Op::Del, ma.len()).chain(std::iter::repeat_n(Op::Add, mb.len())));
    }
    let ops: Vec<Op> = std::iter::repeat_n(Op::Same, pre).chain(mid).chain(std::iter::repeat_n(Op::Same, post)).collect();
    if !ops.contains(&Op::Del) && !ops.contains(&Op::Add) {
        return if old == new { "(no change)".into() } else { "(only the line endings change)".into() };
    }
    // Which ops to show: the changes and two lines either side.
    let near = |k: usize| ops[k.saturating_sub(2)..(k + 3).min(ops.len())].iter().any(|o| *o != Op::Same);
    let mut out = String::new();
    let (mut ia, mut ib, mut shown_last) = (0usize, 0usize, false);
    for (k, op) in ops.iter().enumerate() {
        let show = near(k);
        if show && !shown_last {
            out.push_str(&format!("@@ -{} +{} @@\n", ia + 1, ib + 1));
        }
        match op {
            Op::Same => {
                if show {
                    out.push_str(&format!(" {}\n", a[ia]));
                }
                ia += 1;
                ib += 1;
            }
            Op::Del => {
                out.push_str(&format!("-{}\n", a[ia]));
                ia += 1;
            }
            Op::Add => {
                out.push_str(&format!("+{}\n", b[ib]));
                ib += 1;
            }
        }
        shown_last = show;
        if out.len() > 16 << 10 {
            out.push_str("… (the diff goes on)\n");
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diffs_show_what_changes() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let new = "a\nb\nc\nD\ne\nf\ng\nh\ni\n";
        assert_eq!(diff(old, new), "@@ -2 +2 @@\n b\n c\n-d\n+D\n e\n f\n g\n h\n+i\n");
        let far = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        assert_eq!(diff(far, "1\n2\nthree\n4\n5\n6\n7\n8\nnine\n"), "@@ -1 +1 @@\n 1\n 2\n-3\n+three\n 4\n 5\n@@ -7 +7 @@\n 7\n 8\n-9\n+nine\n");
        assert_eq!(diff("x\n", "x\n"), "(no change)");
        assert_eq!(diff("", "net.ipv4.ip_forward = 1\n"), "@@ -1 +1 @@\n+net.ipv4.ip_forward = 1\n");
        assert_eq!(diff("one\ntwo\n", ""), "@@ -1 +1 @@\n-one\n-two\n");
        let big_old: String = (0..3000).map(|i| format!("{i}\n")).collect();
        let big_new: String = (0..3000).map(|i| format!("{}\n", i * 7)).collect();
        assert!(diff(&big_old, &big_new).ends_with("… (the diff goes on)\n"));
    }

    #[test]
    fn edits_only_where_a_snapshot_reaches() {
        for bad in ["etc/hosts", "/etc/../root/x", "/etc/./hosts", "/proc/sys/kernel/x", "/home/me/.bashrc", "/boot/grub2/grub.cfg", "/tmp/x"] {
            assert!(edit_target(bad).is_err(), "{bad}");
        }
        assert!(format!("{:#}", edit_target("/no-such-dir/x").unwrap_err()).contains("doesn't exist"));
        // /etc is root's alone wherever these tests run.
        let (p, exists) = edit_target("/etc/orion-no-such-file.conf").unwrap();
        assert_eq!((p, exists), (PathBuf::from("/etc/orion-no-such-file.conf"), false));
        assert!(edit_target("/etc").is_err() || !Path::new("/etc").is_file());
        assert!(edit_target("/etc/hostname").is_ok_and(|(_, e)| e) || !Path::new("/etc/hostname").is_file());
        let dir = tempfile::tempdir().unwrap();
        let mine = dir.path().join("x.conf");
        // A folder someone other than root may write is refused (when not running as root).
        if unsafe { libc::geteuid() } != 0 {
            assert!(edit_target(mine.to_str().unwrap()).is_err());
        }
    }

    #[test]
    fn modes_and_programs() {
        assert_eq!(parse_mode("0644").unwrap(), 0o644);
        assert_eq!(parse_mode("600").unwrap(), 0o600);
        for bad in ["4755", "1777", "rw", "0x1", "06444", "08"] {
            assert!(parse_mode(bad).is_err(), "{bad}");
        }
        assert!(root_program("sh").is_ok());
        assert!(root_program("bin/sh").is_err());
        assert!(root_program("/no/such/program").is_err());
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("x.sh");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The temporary folder is open to everyone (or not root's), so not this.
        assert!(root_program(script.to_str().unwrap()).is_err());
    }

    #[test]
    fn only_listed_logins_get_root_work() {
        let cfg = HelperConfig::default();
        assert!(may_root(&cfg, &Peer { pid: 1, uid: 0, gid: 0 }));
        let nobody = Peer { pid: 1, uid: 65534, gid: 65534 };
        assert!(!may_root(&cfg, &nobody));
        let p = Policy::from_toml("").unwrap();
        let refused = format!("{:#}", helper_check_run(&p, &nobody, &["id".into()], None).err().unwrap());
        assert!(refused.contains("root_users"), "{refused}");
        assert!(helper_edit(&p, &nobody, &["/etc/x".into()], Some("")).is_err());
        let listed = HelperConfig { root_users: vec![crate::helper::user_name(65534).unwrap_or_default()], ..Default::default() };
        assert!(may_root(&listed, &nobody));
    }

    #[test]
    fn always_asks_a_person() {
        let p = Policy::from_toml("[defaults]\nwrite = \"allow\"\nexecute = \"allow\"\n[tools]\nroot_run = \"allow\"\n").unwrap();
        for tool in [&RootRun as &dyn Tool, &SysEdit] {
            assert_eq!(p.decide(tool.name(), tool.risk()), Decision::Ask);
            assert!(!tool.preapproval(&p, &json!({})).unasked(Decision::Allow));
            assert!(tool.every_time(&json!({})), "\"always for this session\" doesn't cover {}", tool.name());
        }
        let run = super::super::process::ProcRun;
        assert!(run.every_time(&json!({ "command": "make", "sandbox": false })));
        assert!(!run.every_time(&json!({ "command": "make" })));
    }

    #[test]
    fn refuses_a_sandbox_that_leaks_the_sockets() {
        let landlock = Policy::from_toml("[sandbox]\nmode = \"off\"\n").unwrap();
        assert!(format!("{:#}", sandbox_closed(&landlock).unwrap_err()).contains("bubblewrap"));
        let denied = Policy::from_toml("[sandbox]\nmode = \"off\"\n[tools]\nproc_run = \"deny\"\n").unwrap();
        assert!(sandbox_closed(&denied).is_ok());
    }
}
