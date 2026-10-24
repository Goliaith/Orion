//! Confinement for programs the AI runs (`proc_run`).
//!
//! The strongest backend the machine supports is used:
//!
//! * **bubblewrap**: the whole filesystem read-only except the writable
//!   roots, private paths replaced by empty ones, a private /tmp, its own
//!   process and IPC namespaces, no network unless allowed, no GPU devices
//!   unless allowed.
//! * **Landlock**: kernel access control that needs no privileges or tools:
//!   read anywhere but the private paths, write only in the writable roots
//!   and temporary directories, and (kernel permitting) no TCP and no
//!   signals or abstract sockets outside the sandbox.
//!
//! Either way the program gets resource limits (memory, CPU time, file
//! size, no core dumps), can never gain privileges (no_new_privs), and
//! runs under a seccomp filter that refuses the system calls reaching past
//! the sandbox (see `seccomp`). Where the systemd user manager runs, each
//! program also gets a scope of its own (a cgroup), so its memory limit
//! covers all its processes and their page cache, and a fork bomb stops at
//! `max_processes`.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::policy::{expand_home, Policy};

/// `[sandbox]` in orion.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    /// `auto` (bubblewrap, else Landlock, else limits only), `bubblewrap`,
    /// `landlock`, or `off`.
    pub mode: Mode,
    /// Refuse to run programs when no sandbox backend is available.
    pub require: bool,
    /// Let programs use the network.
    pub network: bool,
    /// Let programs use GPUs (/dev/nvidia*, /dev/dri, /dev/kfd).
    pub gpu: bool,
    /// Heap and other private memory per program, in MiB (0: no limit).
    pub memory_mb: u64,
    /// CPU time per program, in seconds (0: no limit).
    pub cpu_secs: u64,
    /// Largest file a program may write, in MiB (0: no limit).
    pub max_file_mb: u64,
    /// Paths programs never see. `chat.router.private_paths` are added.
    pub hide: Vec<PathBuf>,
    /// Refuse the system calls that reach past the sandbox (ptrace, kernel
    /// modules, mounts, new user namespaces, io_uring, …).
    pub seccomp: bool,
    /// Run each program in a systemd scope of its own (needs the systemd
    /// user manager), so memory_mb covers all of it, page cache included.
    pub cgroup: bool,
    /// Processes and threads per program, in its scope (0: no limit).
    pub max_processes: u64,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Auto,
            require: false,
            network: false,
            gpu: false,
            memory_mb: 8192,
            cpu_secs: 300,
            max_file_mb: 4096,
            hide: [
                "~/.ssh",
                "~/.gnupg",
                "~/.password-store",
                "~/.aws",
                "~/.netrc",
                "~/.config/orion",
            ]
            .iter()
            .map(PathBuf::from)
            .collect(),
            seccomp: true,
            cgroup: true,
            max_processes: 512,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Auto,
    Bubblewrap,
    Landlock,
    Off,
}

/// The backend a run will use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Bubblewrap,
    Landlock(u32),
    /// Resource limits and no_new_privs only.
    LimitsOnly,
    Off,
}

impl Backend {
    pub fn name(self) -> String {
        match self {
            Backend::Bubblewrap => "bubblewrap".into(),
            Backend::Landlock(abi) => format!("landlock (ABI {abi})"),
            Backend::LimitsOnly => "limits only".into(),
            Backend::Off => "off".into(),
        }
    }
}

/// The backend `config` selects on this machine.
pub fn backend(config: &SandboxConfig) -> Result<Backend> {
    let abi = landlock_abi();
    let chosen = match config.mode {
        Mode::Off => return Ok(Backend::Off),
        Mode::Bubblewrap if bubblewrap_works() => Backend::Bubblewrap,
        Mode::Bubblewrap => {
            bail!("sandbox.mode is bubblewrap, but bwrap is missing or can't create a sandbox here")
        }
        Mode::Landlock if abi > 0 => Backend::Landlock(abi),
        Mode::Landlock => {
            bail!("sandbox.mode is landlock, but this kernel doesn't support Landlock")
        }
        Mode::Auto if bubblewrap_works() => Backend::Bubblewrap,
        Mode::Auto if abi > 0 => Backend::Landlock(abi),
        Mode::Auto => Backend::LimitsOnly,
    };
    if chosen == Backend::LimitsOnly && config.require {
        bail!("no sandbox is available (install bubblewrap, or use a kernel with Landlock), and sandbox.require is set");
    }
    Ok(chosen)
}

/// One line for status displays and the model's instructions.
pub fn describe(policy: &Policy) -> String {
    let c = &policy.sandbox;
    match backend(c) {
        Ok(Backend::Off) => "off: programs run unconfined".into(),
        Ok(b) => {
            let mut parts = vec![b.name()];
            if b != Backend::LimitsOnly {
                parts.push("read-only outside the writable roots".into());
                parts.push("private paths hidden".into());
            }
            parts.push(if c.network {
                "network allowed".into()
            } else {
                network_note(b)
            });
            if !c.gpu && b != Backend::LimitsOnly {
                parts.push("no GPU".into());
            }
            if c.seccomp && !crate::seccomp::program().is_empty() {
                parts.push("system calls filtered".into());
            }
            if c.cgroup && scope_works() {
                parts.push("a cgroup per program".into());
            }
            parts.join(", ")
        }
        Err(e) => format!("unavailable: {e:#}"),
    }
}

fn network_note(b: Backend) -> String {
    match b {
        Backend::Bubblewrap => "no network".into(),
        Backend::Landlock(abi) if abi >= 4 => "no TCP".into(),
        _ => "network not restricted".into(),
    }
}

/// Everything programs must never see.
pub fn hidden_paths(policy: &Policy) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    // Orion's own state too: conversations, the audit log, the shell's
    // token and the voiceprint are nothing a program needs.
    let state = Policy::state_dir();
    for p in policy
        .sandbox
        .hide
        .iter()
        .chain(&policy.chat.router.private_paths)
        .chain(state.as_ref())
    {
        let p = expand_home(p);
        let p = p.canonicalize().unwrap_or(p);
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Writable roots and temporary directories that exist.
fn writable(policy: &Policy) -> Vec<PathBuf> {
    policy
        .fs
        .write_roots
        .iter()
        .filter(|p| p.exists())
        .cloned()
        .collect()
}

fn gpu_devices() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/dev") {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("nvidia") || name == "kfd" || name == "dri" {
                out.push(e.path());
            }
        }
    }
    out.sort();
    out
}

/// Build the command that runs `program` under `policy`'s sandbox.
/// Returns the command and the backend used. Keep the returned guard alive
/// until the program has been spawned.
pub fn command(
    policy: &Policy,
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
) -> Result<(Command, Backend, Guard)> {
    let config = &policy.sandbox;
    let b = backend(config)?;
    let limits = Limits {
        memory: config.memory_mb.saturating_mul(1 << 20),
        cpu: config.cpu_secs,
        file: config.max_file_mb.saturating_mul(1 << 20),
    };
    let mut guard = Guard {
        _ruleset: None,
        _seccomp: None,
    };
    let filter = if config.seccomp && b != Backend::Off {
        crate::seccomp::program()
    } else {
        Vec::new()
    };
    // In a scope of its own, when the user manager can make one.
    let scoped = config.cgroup && b != Backend::Off && scope_works();
    let start = |program: &str, args: &[String]| -> Command {
        if !scoped {
            let mut c = Command::new(program);
            c.args(args);
            return c;
        }
        let mut c = Command::new("systemd-run");
        c.args(["--user", "--scope", "--quiet", "--collect"]);
        if config.memory_mb > 0 {
            c.args(["-p".into(), format!("MemoryMax={}M", config.memory_mb)]);
            c.args(["-p", "MemorySwapMax=0"]);
        }
        if config.max_processes > 0 {
            c.args(["-p".into(), format!("TasksMax={}", config.max_processes)]);
        }
        c.arg("--").arg(program).args(args);
        c
    };
    let mut cmd = match b {
        Backend::Bubblewrap => {
            let mut a = bwrap_args(policy, cwd);
            if !filter.is_empty() {
                // bwrap installs it just before running the program.
                let fd = seccomp_fd(&crate::seccomp::bytes())?;
                a.extend(["--seccomp".into(), fd.as_raw_fd().to_string()]);
                guard._seccomp = Some(fd);
            }
            a.push("--".into());
            a.push(program.into());
            a.extend(args.iter().cloned());
            let mut c = start("bwrap", &a);
            // bwrap sets no_new_privs for the program itself; it may need
            // privileges (setuid builds) to set up.
            limits.apply(&mut c, None, false, Vec::new());
            c
        }
        Backend::Landlock(abi) => {
            let ruleset = landlock_ruleset(policy, abi)?;
            let mut c = start(program, args);
            if let Some(dir) = cwd {
                c.current_dir(dir);
            }
            limits.apply(&mut c, Some(ruleset.as_raw_fd()), true, filter);
            guard._ruleset = Some(ruleset);
            c
        }
        Backend::LimitsOnly => {
            let mut c = start(program, args);
            if let Some(dir) = cwd {
                c.current_dir(dir);
            }
            limits.apply(&mut c, None, true, filter);
            c
        }
        Backend::Off => {
            let mut c = Command::new(program);
            c.args(args);
            if let Some(dir) = cwd {
                c.current_dir(dir);
            }
            c
        }
    };
    cmd.process_group(0);
    Ok((cmd, b, guard))
}

/// Holds sandbox resources (the Landlock ruleset, bwrap's seccomp
/// program) until spawn.
pub struct Guard {
    _ruleset: Option<OwnedFd>,
    _seccomp: Option<OwnedFd>,
}

/// An inheritable memory file holding `bytes`, read from the start, for
/// bwrap's `--seccomp FD`.
fn seccomp_fd(bytes: &[u8]) -> Result<OwnedFd> {
    let name = CString::new("orion-seccomp").expect("no NUL");
    // SAFETY: a valid name; the new descriptor is owned below. No
    // MFD_CLOEXEC: bwrap must inherit it.
    let raw = unsafe { libc::memfd_create(name.as_ptr(), 0) };
    if raw < 0 {
        bail!("memfd_create: {}", std::io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh descriptor nobody else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut file = std::fs::File::from(fd.try_clone()?);
    use std::io::{Seek, Write};
    file.write_all(bytes)?;
    file.rewind()?;
    Ok(fd)
}

/// Whether the systemd user manager makes scopes here (checked once).
pub fn scope_works() -> bool {
    static WORKS: OnceLock<bool> = OnceLock::new();
    *WORKS.get_or_init(|| {
        let mut c = Command::new("systemd-run");
        c.args(["--user", "--scope", "--quiet", "--collect", "-p", "TasksMax=16", "--", "true"]);
        crate::tools::exec::run(c, None, Duration::from_secs(10), 4096).is_ok_and(|d| d.success())
    })
}

struct Limits {
    memory: u64,
    cpu: u64,
    file: u64,
}

impl Limits {
    fn apply(&self, cmd: &mut Command, landlock: Option<i32>, no_new_privs: bool, filter: Vec<libc::sock_filter>) {
        let (memory, cpu, file) = (self.memory, self.cpu, self.file);
        // SAFETY: runs in the child between fork and exec, and only makes
        // async-signal-safe system calls.
        unsafe {
            cmd.pre_exec(move || {
                let set = |resource, value: u64| {
                    let lim = libc::rlimit {
                        rlim_cur: value as libc::rlim_t,
                        rlim_max: value as libc::rlim_t,
                    };
                    libc::setrlimit(resource, &lim)
                };
                if memory > 0 {
                    set(libc::RLIMIT_DATA, memory);
                }
                if cpu > 0 {
                    set(libc::RLIMIT_CPU, cpu);
                }
                if file > 0 {
                    set(libc::RLIMIT_FSIZE, file);
                }
                set(libc::RLIMIT_CORE, 0);
                if no_new_privs && libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if let Some(fd) = landlock {
                    if libc::syscall(libc::SYS_landlock_restrict_self, fd, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                // Last: the filter was built before the fork, and needs no_new_privs.
                crate::seccomp::install(&filter)?;
                Ok(())
            });
        }
    }
}

fn bwrap_args(policy: &Policy, cwd: Option<&Path>) -> Vec<String> {
    let c = &policy.sandbox;
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let mut a: Vec<String> = [
        "--die-with-parent",
        "--new-session",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-cgroup-try",
        "--cap-drop",
        "ALL",
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--tmpfs",
        "/tmp",
        "--tmpfs",
        "/dev/shm",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if !c.network {
        a.push("--unshare-net".into());
    }
    if c.gpu {
        for d in gpu_devices() {
            a.extend(["--dev-bind-try".into(), s(&d), s(&d)]);
        }
    }
    for w in writable(policy) {
        a.extend(["--bind".into(), s(&w), s(&w)]);
    }
    // Last, so hiding wins even inside a writable root.
    for h in hidden_paths(policy) {
        if h.is_dir() {
            a.extend(["--tmpfs".into(), s(&h)]);
        } else if h.exists() {
            a.extend(["--ro-bind".into(), "/dev/null".into(), s(&h)]);
        }
    }
    if let Some(dir) = cwd {
        a.extend(["--chdir".into(), s(dir)]);
    }
    a
}

/// Whether bwrap can create a sandbox here (checked once).
pub fn bubblewrap_works() -> bool {
    static WORKS: OnceLock<bool> = OnceLock::new();
    *WORKS.get_or_init(|| {
        let child = Command::new("bwrap")
            .args([
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--unshare-pid",
                "--unshare-net",
                "true",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else { return false };
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if start.elapsed() < Duration::from_secs(5) => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => {
                    let _ = child.kill();
                    return false;
                }
            }
        }
    })
}

// Landlock, from linux/landlock.h.
const ACCESS_FS_EXECUTE: u64 = 1 << 0;
const ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const ACCESS_FS_READ_FILE: u64 = 1 << 2;
const ACCESS_FS_READ_DIR: u64 = 1 << 3;
const ACCESS_FS_REFER: u64 = 1 << 13;
const ACCESS_FS_TRUNCATE: u64 = 1 << 14;
const ACCESS_FS_IOCTL_DEV: u64 = 1 << 15;
const ACCESS_NET_BIND_TCP: u64 = 1 << 0;
const ACCESS_NET_CONNECT_TCP: u64 = 1 << 1;
const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;
const RULE_PATH_BENEATH: libc::c_int = 1;
const CREATE_RULESET_VERSION: u32 = 1;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The kernel's Landlock ABI version; 0 when unsupported.
pub fn landlock_abi() -> u32 {
    // SAFETY: the documented way to query the ABI version.
    let v = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    v.max(0) as u32
}

/// Filesystem rights this ABI knows.
fn fs_rights(abi: u32) -> u64 {
    let mut all = (1 << 13) - 1; // execute .. make_sym (ABI 1)
    if abi >= 2 {
        all |= ACCESS_FS_REFER;
    }
    if abi >= 3 {
        all |= ACCESS_FS_TRUNCATE;
    }
    if abi >= 5 {
        all |= ACCESS_FS_IOCTL_DEV;
    }
    all
}

/// Rights that make sense on a file rather than a directory.
const FILE_RIGHTS: u64 = ACCESS_FS_EXECUTE
    | ACCESS_FS_WRITE_FILE
    | ACCESS_FS_READ_FILE
    | ACCESS_FS_TRUNCATE
    | ACCESS_FS_IOCTL_DEV;

fn landlock_ruleset(policy: &Policy, abi: u32) -> Result<OwnedFd> {
    let all = fs_rights(abi);
    let read = ACCESS_FS_EXECUTE | ACCESS_FS_READ_FILE | ACCESS_FS_READ_DIR;
    let mut attr = RulesetAttr {
        handled_access_fs: all,
        handled_access_net: 0,
        scoped: 0,
    };
    if abi >= 4 && !policy.sandbox.network {
        attr.handled_access_net = ACCESS_NET_BIND_TCP | ACCESS_NET_CONNECT_TCP;
    }
    if abi >= 6 {
        attr.scoped = SCOPE_SIGNAL | SCOPE_ABSTRACT_UNIX_SOCKET;
    }
    let size = match abi {
        0..=3 => 8,
        4..=5 => 16,
        _ => std::mem::size_of::<RulesetAttr>(),
    };
    // SAFETY: `attr` is valid for `size` bytes.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const RulesetAttr,
            size,
            0u32,
        )
    };
    if fd < 0 {
        bail!(
            "creating a Landlock ruleset: {}",
            std::io::Error::last_os_error()
        );
    }
    // SAFETY: a fresh descriptor we own.
    let ruleset = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let hidden = hidden_paths(policy);

    // Read and run anything but the hidden paths.
    allow_except(&ruleset, Path::new("/"), read, &hidden)?;
    // Write in the writable roots, temporary directories, and the
    // harmless devices.
    let mut write: Vec<PathBuf> = writable(policy);
    write.extend(
        ["/tmp", "/var/tmp", "/dev/shm"]
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.exists()),
    );
    for w in write {
        allow_except(&ruleset, &w, all, &hidden)?;
    }
    for dev in [
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/random",
        "/dev/urandom",
        "/dev/tty",
        "/dev/ptmx",
    ] {
        let _ = add_rule(
            &ruleset,
            Path::new(dev),
            read | ACCESS_FS_WRITE_FILE | (all & ACCESS_FS_IOCTL_DEV),
        );
    }
    let _ = add_rule(&ruleset, Path::new("/dev/pts"), all);
    if policy.sandbox.gpu {
        for d in gpu_devices() {
            let _ = add_rule(&ruleset, &d, all);
        }
    }
    Ok(ruleset)
}

/// Allow `rights` beneath `path`, except beneath any of `hidden`: where a
/// hidden path lies inside, allow each sibling instead, recursively.
fn allow_except(ruleset: &OwnedFd, path: &Path, rights: u64, hidden: &[PathBuf]) -> Result<()> {
    if hidden.iter().any(|h| path.starts_with(h)) {
        return Ok(());
    }
    if !hidden.iter().any(|h| h.starts_with(path) && h != path) {
        return add_rule(ruleset, path, rights);
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return Ok(());
    };
    // The directory itself can be listed, but its entries are covered one by one.
    let _ = add_rule(
        ruleset,
        path,
        rights & (ACCESS_FS_READ_DIR | ACCESS_FS_EXECUTE),
    );
    for e in entries.flatten() {
        allow_except(ruleset, &e.path(), rights, hidden)?;
    }
    Ok(())
}

fn add_rule(ruleset: &OwnedFd, path: &Path, rights: u64) -> Result<()> {
    let c = CString::new(path.as_os_str().as_encoded_bytes())?;
    // SAFETY: opening a path for use as a Landlock anchor.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        // Gone or unreadable: nothing to allow.
        return Ok(());
    }
    // SAFETY: we own this descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let is_dir = std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false);
    let allowed = if is_dir { rights } else { rights & FILE_RIGHTS };
    if allowed == 0 {
        return Ok(());
    }
    let attr = PathBeneathAttr {
        allowed_access: allowed,
        parent_fd: fd.as_raw_fd(),
    };
    // SAFETY: `attr` is a valid path_beneath rule.
    let r = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd(),
            RULE_PATH_BENEATH,
            &attr as *const PathBeneathAttr,
            0u32,
        )
    };
    if r != 0 {
        bail!(
            "Landlock rule for {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::TcpListener;

    use super::*;

    fn policy(dir: &Path, mode: &str, extra: &str) -> Policy {
        let d = dir.to_str().unwrap();
        Policy::from_toml(&format!(
            "[fs]\nwrite_roots = [\"{d}/rw\"]\n[sandbox]\nmode = \"{mode}\"\nhide = [\"{d}/secret\"]\n{extra}\n[audit]\nenabled = false\n"
        ))
        .unwrap()
    }

    fn setup() -> tempfile::TempDir {
        // Not under /tmp: bubblewrap gives programs a private /tmp.
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        std::fs::create_dir_all(&base).unwrap();
        let dir = tempfile::tempdir_in(base).unwrap();
        std::fs::create_dir_all(dir.path().join("rw")).unwrap();
        std::fs::create_dir_all(dir.path().join("ro")).unwrap();
        std::fs::create_dir_all(dir.path().join("secret")).unwrap();
        std::fs::write(dir.path().join("secret/key"), "hunter2").unwrap();
        std::fs::write(dir.path().join("ro/readme"), "hello").unwrap();
        dir
    }

    /// Run `sh -c script` sandboxed; returns (exit code, stdout+stderr).
    fn sh(policy: &Policy, script: &str) -> (i32, String) {
        let (mut cmd, _, guard) =
            command(policy, "sh", &["-c".into(), script.into()], None).unwrap();
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        drop(guard);
        let mut out = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        (child.wait().unwrap().code().unwrap_or(-1), out)
    }

    fn confines(mode: &str) {
        let dir = setup();
        let d = dir.path().display();
        let p = policy(dir.path(), mode, "");
        assert_eq!(
            sh(&p, &format!("cat {d}/ro/readme")),
            (0, "hello".into()),
            "reads are allowed"
        );
        assert_eq!(
            sh(&p, &format!("echo hi > {d}/rw/out && cat {d}/rw/out")).0,
            0,
            "writes in a writable root"
        );
        assert_ne!(
            sh(&p, &format!("echo hi > {d}/ro/out")).0,
            0,
            "no writes elsewhere"
        );
        assert!(!dir.path().join("ro/out").exists());
        let (code, out) = sh(&p, &format!("cat {d}/secret/key"));
        assert!(
            code != 0 || out.is_empty(),
            "hidden paths stay hidden: {out}"
        );
        assert!(!out.contains("hunter2"));
        assert!(
            sh(&p, "grep -q 'NoNewPrivs:\t1' /proc/self/status").0 == 0,
            "no new privileges"
        );

        // No network: connecting to a local listener fails.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let script = format!(
            "python3 -c \"import socket; socket.create_connection(('127.0.0.1', {port}), 2)\""
        );
        assert_ne!(sh(&p, &script).0, 0, "no TCP");
        let open = policy(dir.path(), mode, "network = true");
        assert_eq!(sh(&open, &script).0, 0, "network = true allows it");
    }

    #[test]
    fn landlock_confines_programs() {
        if landlock_abi() < 4 {
            eprintln!("skipped: Landlock ABI 4+ needed");
            return;
        }
        confines("landlock");
    }

    #[test]
    fn bubblewrap_confines_programs() {
        if !bubblewrap_works() {
            eprintln!("skipped: bubblewrap unavailable");
            return;
        }
        confines("bubblewrap");
    }

    #[test]
    fn landlock_scopes_signals() {
        if landlock_abi() < 6 {
            return;
        }
        let dir = setup();
        let p = policy(dir.path(), "landlock", "");
        let me = std::process::id();
        assert_ne!(
            sh(&p, &format!("kill -0 {me}")).0,
            0,
            "can't signal processes outside the sandbox"
        );
        assert_eq!(sh(&p, "kill -0 $$").0, 0, "but can signal itself");
    }

    #[test]
    fn limits_apply_everywhere() {
        let dir = setup();
        for mode in ["landlock", "bubblewrap", "auto"] {
            if (mode == "landlock" && landlock_abi() == 0)
                || (mode == "bubblewrap" && !bubblewrap_works())
            {
                continue;
            }
            let p = policy(dir.path(), mode, "cpu_secs = 1\nmax_file_mb = 1");
            let start = Instant::now();
            let (code, _) = sh(&p, "while :; do :; done");
            assert_ne!(code, 0, "{mode}: CPU limit stops a busy loop");
            assert!(start.elapsed() < Duration::from_secs(10));
            let (code, _) = sh(
                &p,
                &format!(
                    "head -c 3000000 /dev/zero > {}/rw/big",
                    dir.path().display()
                ),
            );
            assert_ne!(code, 0, "{mode}: file size limit");
        }
    }

    #[test]
    fn filters_system_calls() {
        if !Path::new("/usr/bin/python3").exists() {
            return;
        }
        let dir = setup();
        // keyctl(KEYCTL_GET_KEYRING_ID, KEY_SPEC_SESSION_KEYRING): harmless, and refused.
        let script = format!(
            "python3 -c 'import ctypes; l = ctypes.CDLL(None, use_errno=True); r = l.syscall({}, 0, -3, 0); print(r, ctypes.get_errno())'",
            libc::SYS_keyctl
        );
        for mode in ["landlock", "bubblewrap"] {
            if (mode == "landlock" && landlock_abi() == 0) || (mode == "bubblewrap" && !bubblewrap_works()) {
                continue;
            }
            let on = policy(dir.path(), mode, "");
            assert_eq!(sh(&on, &script).1.trim(), "-1 1", "{mode}: keyctl fails with EPERM");
            assert_eq!(sh(&on, "echo fine; ls / >/dev/null && echo still"), (0, "fine\nstill\n".into()), "{mode}: ordinary programs run");
            let off = policy(dir.path(), mode, "seccomp = false");
            assert_ne!(sh(&off, &script).1.trim(), "-1 1", "{mode}: without the filter it works");
        }
    }

    #[test]
    fn scopes_limit_memory_and_processes() {
        if !scope_works() || landlock_abi() == 0 {
            return;
        }
        let dir = setup();
        let p = policy(dir.path(), "landlock", "memory_mb = 64\nmax_processes = 20");
        let cg = "/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup)";
        let (code, out) = sh(&p, &format!("cat {cg}/memory.max {cg}/pids.max"));
        assert_eq!((code, out.as_str()), (0, "67108864\n20\n"));
        // A fork bomb stops at the limit.
        let (code, _) = sh(&p, "for i in $(seq 40); do sleep 2 & done; wait");
        assert_ne!(code, 0);
        assert!(describe(&p).contains("a cgroup per program"));
    }

    #[test]
    fn selection_and_require() {
        let off = SandboxConfig {
            mode: Mode::Off,
            ..Default::default()
        };
        assert_eq!(backend(&off).unwrap(), Backend::Off);
        assert!(matches!(
            backend(&SandboxConfig::default()).unwrap(),
            Backend::Bubblewrap | Backend::Landlock(_) | Backend::LimitsOnly
        ));
        let p = Policy::from_toml("[sandbox]\nmode = \"off\"\n").unwrap();
        assert!(describe(&p).starts_with("off"));
    }
}
