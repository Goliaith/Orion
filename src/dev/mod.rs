//! Developer mode: a place where the AI can build and test software
//! iteratively, beside the everyday Safe Assistant mode.
//!
//! Safe mode is the default and what Orion goes back to. The person, never
//! the AI, switches with `orion mode`, for a while (`[dev] expires`):
//!
//! - **sandboxed** (`orion mode dev`): a persistent shell (`dev_shell`) in
//!   bubblewrap, writable only in the dev root and the toolchain caches,
//!   with the network on and limits sized for builds. Keys, Orion's files
//!   and sockets, and root stay out of reach.
//! - **unsandboxed** (`orion mode dev --unsandboxed`): the same shell as the
//!   person, with their files, keys and network, in a namespace that still
//!   hides Orion's sockets, the helper's, and the user bus, keeps Orion's
//!   own files out of sight and the login scripts read-only. Best effort,
//!   not a hard wall.
//!
//! Either way the dev root is checkpointed before each command (see
//! [`checkpoint`]), and every command and switch is in the audit log.

pub mod checkpoint;

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};

use crate::policy::{expand_home, Policy};

/// `[dev]` in orion.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DevConfig {
    /// Where projects live, and the only folder the sandboxed shell may
    /// write besides the caches. Created when the mode is turned on.
    pub root: PathBuf,
    /// Toolchain caches the sandboxed shell may write, where they exist.
    pub caches: Vec<PathBuf>,
    /// The network in the sandboxed shell.
    pub network: bool,
    /// GPUs in the sandboxed shell.
    pub gpu: bool,
    /// Memory for a whole shell and what it runs, in MiB (0: no limit).
    pub memory_mb: u64,
    /// Processes and threads in a shell (0: no limit).
    pub max_processes: u64,
    /// The longest one `dev_shell` call waits for its command; a command
    /// still running then keeps running, and `dev_shell_read` follows it.
    pub wait_secs: u64,
    /// How long `orion mode dev` lasts unless told otherwise: `8h`, `30m`,
    /// `2d` or `never`.
    pub expires: String,
    /// The same for `orion mode dev --unsandboxed`.
    pub unsandboxed_expires: String,
    /// Allow `orion mode dev --unsandboxed` at all.
    pub unsandboxed: bool,
    /// Under `--unsandboxed`, paths the shell may read but not change: what
    /// runs at the next login, outside the shell.
    pub protect: Vec<PathBuf>,
    /// Checkpoints of the dev root kept for `dev_rollback` (0: none taken).
    pub checkpoints: usize,
    /// Folder names checkpoints leave out, wherever they are: build output
    /// and downloaded dependencies, which a build makes again.
    pub skip: Vec<String>,
    /// Files larger than this are left out of checkpoints, in MiB.
    pub max_file_mb: u64,
}

impl Default for DevConfig {
    fn default() -> Self {
        let paths = |v: &[&str]| v.iter().map(PathBuf::from).collect();
        Self {
            root: PathBuf::from("~/orion-dev"),
            caches: paths(&[
                "~/.cargo",
                "~/.rustup",
                "~/.npm",
                "~/.cache/pip",
                "~/.cache/pypoetry",
                "~/.cache/go-build",
                "~/go",
                "~/.m2",
                "~/.gradle",
            ]),
            network: true,
            gpu: false,
            memory_mb: 16384,
            max_processes: 4096,
            wait_secs: 1800,
            expires: "8h".into(),
            unsandboxed_expires: "1h".into(),
            unsandboxed: true,
            protect: paths(&[
                "~/.bashrc",
                "~/.bash_profile",
                "~/.bash_login",
                "~/.profile",
                "~/.zshrc",
                "~/.zprofile",
                "~/.config/fish",
                "~/.config/systemd",
                "~/.config/autostart",
                "~/.config/environment.d",
                "~/.local/share/systemd",
                "~/.ssh",
                "~/.gnupg",
                "~/.pam_environment",
                "~/.xprofile",
                "~/.xinitrc",
            ]),
            checkpoints: 50,
            skip: ["target", "node_modules", ".venv", "venv", "__pycache__", ".gradle", ".zig-cache", ".mypy_cache", ".pytest_cache"]
                .map(String::from)
                .to_vec(),
            max_file_mb: 64,
        }
    }
}

impl DevConfig {
    /// The dev root, `~` expanded.
    pub fn root(&self) -> PathBuf {
        let p = expand_home(&self.root);
        p.canonicalize().unwrap_or(p)
    }
}

/// Which mode Orion is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Safe,
    Sandboxed,
    Unsandboxed,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Safe => "Safe Assistant mode",
            Mode::Sandboxed => "Developer mode (sandboxed)",
            Mode::Unsandboxed => "Developer mode (UNSANDBOXED)",
        }
    }
}

/// What `orion mode` last set, in the state folder (which sandboxed
/// programs never see).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub mode: Mode,
    /// Unix seconds.
    pub since: u64,
    /// Unix seconds; `None`: until switched off.
    pub until: Option<u64>,
}

impl State {
    fn safe() -> Self {
        Self { mode: Mode::Safe, since: now(), until: None }
    }

    /// The mode in force now: safe once it has expired.
    pub fn mode(&self) -> Mode {
        match self.until {
            Some(until) if until <= now() => Mode::Safe,
            _ => self.mode,
        }
    }

    /// One line for status displays.
    pub fn describe(&self) -> String {
        let mode = self.mode();
        if mode == Mode::Safe {
            return Mode::Safe.name().into();
        }
        match self.until {
            Some(until) => format!("{}, for {} more", mode.name(), human(until.saturating_sub(now()))),
            None => format!("{}, until switched off", mode.name()),
        }
    }
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// `3h 20m`, `45m`, `2d 4h`.
pub fn human(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs / 3600 % 24, secs.div_ceil(60) % 60);
    match (d, h) {
        (0, 0) => format!("{}m", secs.div_ceil(60).max(1)),
        (0, h) if m == 0 => format!("{h}h"),
        (0, h) => format!("{h}h {m}m"),
        (d, 0) => format!("{d}d"),
        (d, h) => format!("{d}d {h}h"),
    }
}

/// `8h`, `30m`, `90s`, `2d`, or `never` (`None`).
pub fn parse_duration(s: &str) -> Result<Option<u64>> {
    let s = s.trim();
    if s == "never" {
        return Ok(None);
    }
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = n.parse().with_context(|| format!("{s:?} is not a duration such as 8h, 30m or never"))?;
    let secs = match unit {
        "s" => n,
        "m" | "" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => bail!("{s:?} is not a duration such as 8h, 30m or never"),
    };
    ensure!(secs > 0, "a duration must be more than nothing");
    Ok(Some(secs))
}

fn state_path() -> Option<PathBuf> {
    Policy::state_dir().map(|d| d.join("dev-mode.json"))
}

/// The mode, as last set.
pub fn state() -> State {
    state_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(State::safe)
}

/// The mode in force now.
pub fn mode() -> Mode {
    state().mode()
}

/// Set the mode. `lasts`: `None` for the configured default.
pub fn switch(policy: &Policy, mode: Mode, lasts: Option<&str>) -> Result<State> {
    let c = &policy.dev;
    let until = match mode {
        Mode::Safe => None,
        Mode::Sandboxed => parse_duration(lasts.unwrap_or(&c.expires))?,
        Mode::Unsandboxed => {
            ensure!(c.unsandboxed, "unsandboxed developer mode is off ([dev] unsandboxed = false)");
            parse_duration(lasts.unwrap_or(&c.unsandboxed_expires))?
        }
    };
    if mode != Mode::Safe {
        // Both modes stand on bubblewrap: Landlock can't hide the sockets.
        ensure!(
            crate::sandbox::bubblewrap_works(),
            "developer mode needs bubblewrap, which is missing or can't make a sandbox here (on Fedora: sudo dnf install bubblewrap)"
        );
        let root = expand_home(&c.root);
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
    }
    let state = State { mode, since: now(), until: until.map(|s| now() + s) };
    let path = state_path().context("no home folder to keep the mode in")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(&state)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(state)
}

/// `orion mode`: switch to `mode`, or with `None` say which is on.
/// Turning developer mode on takes a person at a terminal; turning it off
/// doesn't.
pub fn command(policy: &Policy, mode: Option<Mode>, lasts: Option<&str>) -> Result<()> {
    let Some(mode) = mode else {
        let s = state();
        println!("{}", s.describe());
        if s.mode() == Mode::Safe && s.mode != Mode::Safe {
            println!("(developer mode ran out)");
        }
        println!("dev root: {}", policy.dev.root().display());
        return Ok(());
    };
    // SAFETY: isatty has no preconditions.
    let at_terminal = unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 };
    ensure!(
        mode == Mode::Safe || at_terminal,
        "developer mode is turned on by a person at a terminal: run `orion mode dev` there"
    );
    let before = state().mode();
    let s = switch(policy, mode, lasts)?;
    crate::Orion::new(policy.clone()).record(
        "dev_mode",
        &serde_json::json!({ "from": before, "to": s.mode, "until": s.until }),
        Some("orion mode"),
        None,
    );
    println!("{}", s.describe());
    match mode {
        Mode::Safe => println!("Dev shells close within a few seconds."),
        Mode::Sandboxed => {
            println!("The AI can open a shell in {} (you approve that once), and its commands run there", policy.dev.root().display());
            println!("without asking: network on, your keys, Orion's files and root out of reach.");
            println!("`orion mode safe` ends it; dev_rollback undoes what a command did in the dev root.");
        }
        Mode::Unsandboxed => {
            println!("The AI can open a shell that runs AS YOU, with your files, keys and network. You approve each shell;");
            println!("its commands then run without asking. Orion's files and sockets, the root helper and the user bus");
            println!("stay hidden, and login scripts are read-only, but this is best effort, not a sandbox.");
            println!("`orion mode safe` ends it; dev_rollback undoes only what a command did in the dev root.");
        }
    }
    Ok(())
}

/// The policy a dev shell's programs run under: the person's, with the
/// sandbox opened as far as `mode` says.
pub fn shell_policy(policy: &Policy, mode: Mode) -> Result<Policy> {
    let c = &policy.dev;
    let root = c.root();
    ensure!(root.is_dir(), "the dev root {} doesn't exist; `orion mode dev` makes it", root.display());
    let mut p = policy.clone();
    p.sandbox.mode = crate::sandbox::Mode::Bubblewrap;
    p.sandbox.require = true;
    p.sandbox.network = c.network;
    p.sandbox.gpu = c.gpu;
    p.sandbox.memory_mb = c.memory_mb;
    p.sandbox.max_processes = c.max_processes;
    // A build runs for as long as it runs; the scope's memory still holds.
    p.sandbox.cpu_secs = 0;
    p.sandbox.max_file_mb = 0;
    match mode {
        Mode::Safe => bail!("developer mode is off"),
        Mode::Sandboxed => {
            let mut roots = vec![root];
            roots.extend(c.caches.iter().map(|d| expand_home(d)).filter(|d| d.is_dir()).map(|d| d.canonicalize().unwrap_or(d)));
            p.fs.write_roots = roots;
            p.sandbox.dev = Some(crate::sandbox::DevProfile { unsandboxed: false, protect: Vec::new() });
        }
        Mode::Unsandboxed => {
            let protect = c.protect.iter().map(|d| expand_home(d)).filter(|d| d.exists()).collect();
            p.sandbox.network = true;
            p.sandbox.gpu = true;
            p.sandbox.dev = Some(crate::sandbox::DevProfile { unsandboxed: true, protect });
        }
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("8h").unwrap(), Some(8 * 3600));
        assert_eq!(parse_duration("30m").unwrap(), Some(1800));
        assert_eq!(parse_duration("45").unwrap(), Some(45 * 60));
        assert_eq!(parse_duration("2d").unwrap(), Some(2 * 86_400));
        assert_eq!(parse_duration("never").unwrap(), None);
        assert!(parse_duration("0h").is_err() && parse_duration("soon").is_err() && parse_duration("8x").is_err());
        assert_eq!((human(59), human(3600), human(3600 + 20 * 60), human(86_400 + 7200)), ("1m".into(), "1h".into(), "1h 20m".into(), "1d 2h".into()));
    }

    #[test]
    fn the_example_shows_the_defaults() {
        let p = Policy::from_toml(include_str!("../../orion.example.toml")).unwrap();
        assert_eq!(format!("{:?}", p.dev), format!("{:?}", DevConfig::default()));
    }

    #[test]
    fn modes_expire() {
        let s = State { mode: Mode::Sandboxed, since: 0, until: Some(now() - 1) };
        assert_eq!(s.mode(), Mode::Safe);
        let s = State { mode: Mode::Unsandboxed, since: 0, until: Some(now() + 600) };
        assert_eq!(s.mode(), Mode::Unsandboxed);
        assert!(s.describe().contains("UNSANDBOXED") && s.describe().contains("10m"), "{}", s.describe());
        assert_eq!(State { mode: Mode::Sandboxed, since: 0, until: None }.mode(), Mode::Sandboxed);
    }

    #[test]
    fn the_shell_policy_opens_only_the_dev_root_and_caches() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("dev");
        std::fs::create_dir(&root).unwrap();
        let cache = dir.path().join("cargo");
        std::fs::create_dir(&cache).unwrap();
        let text = format!(
            "[dev]\nroot = {:?}\ncaches = [{:?}, {:?}]\n[audit]\nenabled = false\n",
            root.to_str().unwrap(),
            cache.to_str().unwrap(),
            dir.path().join("missing").to_str().unwrap()
        );
        let p = Policy::from_toml(&text).unwrap();
        let s = shell_policy(&p, Mode::Sandboxed).unwrap();
        assert_eq!(s.fs.write_roots, vec![root.canonicalize().unwrap(), cache.canonicalize().unwrap()]);
        assert!(s.sandbox.network && s.sandbox.require && s.sandbox.cpu_secs == 0);
        assert!(!s.sandbox.dev.as_ref().unwrap().unsandboxed);
        // The person's own policy is left as it was.
        assert!(!p.sandbox.network);
        assert!(shell_policy(&p, Mode::Safe).is_err());
        let u = shell_policy(&p, Mode::Unsandboxed).unwrap();
        assert!(u.sandbox.dev.as_ref().unwrap().unsandboxed);
    }
}
