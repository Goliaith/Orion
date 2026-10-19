use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};

use crate::tool::Risk;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    /// Allowed only if a human approves the call when it is made. With no
    /// human present (`orion serve`), this acts as `deny`.
    Ask,
    Deny,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Ask => "ask",
            Decision::Deny => "deny",
        }
    }
}

/// Decision for each risk level, used when a tool has no explicit entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
    pub read: Decision,
    pub write: Decision,
    pub execute: Decision,
}

impl Default for Defaults {
    fn default() -> Self {
        Self { read: Decision::Allow, write: Decision::Ask, execute: Decision::Ask }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FsPolicy {
    /// Trees the AI may read. Write roots are readable too.
    pub read_roots: Vec<PathBuf>,
    /// Trees the AI may write. Writing still needs approval unless `fs_write`
    /// is set to `allow`.
    pub write_roots: Vec<PathBuf>,
    /// Upper bound on a single `fs_read`.
    pub max_read_bytes: u64,
}

impl Default for FsPolicy {
    fn default() -> Self {
        Self {
            read_roots: vec![PathBuf::from("~")],
            write_roots: vec![PathBuf::from("~/orion-workspace")],
            max_read_bytes: 1 << 20,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProcPolicy {
    /// Pre-approved programs, matched exactly against `command`; `"*"` matches
    /// any. When `proc_run` is `allow`, only these run. When it is `ask`, these
    /// run without a prompt and anything else needs approval.
    pub allowed_commands: Vec<String>,
    pub timeout_secs: u64,
    /// Per stream (stdout and stderr are capped separately).
    pub max_output_bytes: usize,
}

impl Default for ProcPolicy {
    fn default() -> Self {
        Self { allowed_commands: Vec::new(), timeout_secs: 30, max_output_bytes: 64 << 10 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuditPolicy {
    pub enabled: bool,
    /// Defaults to `$XDG_STATE_HOME/orion/audit.jsonl`.
    pub path: Option<PathBuf>,
}

impl Default for AuditPolicy {
    fn default() -> Self {
        Self { enabled: true, path: None }
    }
}

/// What the AI is allowed to do. Loaded from `orion.toml`; see
/// `orion.example.toml` for the format.
///
/// Build one with [`Policy::from_toml`] or [`Policy::load`]. Both expand `~`
/// and canonicalize roots, which `Default` alone does not.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub defaults: Defaults,
    /// Per-tool overrides, keyed by tool name.
    pub tools: BTreeMap<String, Decision>,
    pub fs: FsPolicy,
    pub proc: ProcPolicy,
    pub audit: AuditPolicy,
    /// Settings for the `orion chat` front-end.
    pub chat: crate::chat::ChatConfig,
    /// Voice input and output for the chat.
    pub voice: crate::voice::VoiceConfig,
    /// The graphical shell (`orion shell`).
    pub shell: crate::chat::shell::ShellConfig,
    /// Confinement for programs the AI runs.
    pub sandbox: crate::sandbox::SandboxConfig,
    /// The background service (`orion daemon`).
    pub daemon: crate::daemon::DaemonConfig,
}

impl Policy {
    pub fn from_toml(text: &str) -> Result<Self> {
        let mut policy: Policy = toml::from_str(text)?;
        policy.resolve();
        Ok(policy)
    }

    /// Load from `explicit`, else `$ORION_CONFIG`, else
    /// `$XDG_CONFIG_HOME/orion/orion.toml` if it exists, else built-in
    /// defaults. Returns the file that was used, if any.
    pub fn load(explicit: Option<&Path>) -> Result<(Self, Option<PathBuf>)> {
        let path = explicit
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("ORION_CONFIG").map(PathBuf::from))
            .or_else(|| xdg_dir("XDG_CONFIG_HOME", ".config").map(|d| d.join("orion/orion.toml")).filter(|p| p.exists()));
        match path {
            Some(path) => {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let policy = Self::from_toml(&text).with_context(|| format!("parsing {}", path.display()))?;
                Ok((policy, Some(path)))
            }
            None => Ok((Self::from_toml("")?, None)),
        }
    }

    fn resolve(&mut self) {
        for root in self.fs.read_roots.iter_mut().chain(self.fs.write_roots.iter_mut()) {
            let expanded = expand_home(root);
            *root = expanded.canonicalize().unwrap_or(expanded);
        }
        self.audit.path = self.audit.path.as_deref().map(expand_home);
    }

    pub fn decide(&self, tool: &str, risk: Risk) -> Decision {
        if let Some(d) = self.tools.get(tool) {
            return *d;
        }
        match risk {
            Risk::Read => self.defaults.read,
            Risk::Write => self.defaults.write,
            Risk::Execute => self.defaults.execute,
        }
    }

    /// `$XDG_CONFIG_HOME/orion/orion.toml`, whether or not it exists.
    pub fn default_config_path() -> Option<PathBuf> {
        xdg_dir("XDG_CONFIG_HOME", ".config").map(|d| d.join("orion/orion.toml"))
    }

    /// Orion's own state: `$XDG_STATE_HOME/orion`.
    pub fn state_dir() -> Option<PathBuf> {
        xdg_dir("XDG_STATE_HOME", ".local/state").map(|d| d.join("orion"))
    }

    pub fn audit_path(&self) -> Option<PathBuf> {
        if !self.audit.enabled {
            return None;
        }
        self.audit
            .path
            .clone()
            .or_else(|| xdg_dir("XDG_STATE_HOME", ".local/state").map(|d| d.join("orion/audit.jsonl")))
    }

    /// Resolve `raw` to a real path inside a readable root.
    pub fn check_read(&self, raw: &str) -> Result<PathBuf> {
        let path = absolute(raw)?;
        let real = path.canonicalize().with_context(|| format!("cannot access {raw}"))?;
        ensure!(
            within(&real, &self.fs.read_roots) || within(&real, &self.fs.write_roots),
            "{} is outside the readable roots",
            real.display()
        );
        Ok(real)
    }

    /// Resolve `raw` (which need not exist yet) to a real path inside a
    /// writable root.
    pub fn check_write(&self, raw: &str) -> Result<PathBuf> {
        let path = absolute(raw)?;
        let real = resolve_lenient(&path)?;
        ensure!(within(&real, &self.fs.write_roots), "{} is outside the writable roots", real.display());
        Ok(real)
    }

    pub fn command_allowed(&self, command: &str) -> bool {
        self.proc.allowed_commands.iter().any(|c| c == "*" || c == command)
    }
}

fn within(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| path.starts_with(r))
}

fn absolute(raw: &str) -> Result<PathBuf> {
    let path = expand_home(Path::new(raw));
    ensure!(path.is_absolute(), "path must be absolute or start with ~/: {raw}");
    Ok(path)
}

/// Canonicalize the longest existing prefix of `path` and append the rest.
/// Rejects `..` in the non-existent part and dangling symlinks, either of
/// which could otherwise point outside the root that was checked.
pub(crate) fn resolve_lenient(path: &Path) -> Result<PathBuf> {
    if let Ok(real) = path.canonicalize() {
        return Ok(real);
    }
    if path.symlink_metadata().is_ok() {
        bail!("{} is a dangling symlink", path.display());
    }
    let mut tail = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        let Some(name) = cur.file_name() else {
            bail!("invalid path: {}", path.display());
        };
        tail.push(name.to_os_string());
        if !cur.pop() {
            bail!("invalid path: {}", path.display());
        }
        if let Ok(mut real) = cur.canonicalize() {
            real.extend(tail.iter().rev());
            return Ok(real);
        }
    }
}

pub fn expand_home(path: &Path) -> PathBuf {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return path.to_path_buf();
    };
    match path.to_str() {
        Some("~") => home,
        Some(s) if s.starts_with("~/") => home.join(&s[2..]),
        _ => path.to_path_buf(),
    }
}

fn xdg_dir(var: &str, fallback: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(fallback)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_for(dir: &Path) -> Policy {
        Policy::from_toml(&format!(
            "[fs]\nread_roots = [{r:?}]\nwrite_roots = [{w:?}]\n[audit]\nenabled = false\n",
            r = dir.join("ro").to_str().unwrap(),
            w = dir.join("rw").to_str().unwrap(),
        ))
        .unwrap()
    }

    #[test]
    fn decisions_fall_back_to_risk_defaults() {
        let p = Policy::from_toml("[tools]\nfs_write = \"allow\"\nsystem_info = \"deny\"\n").unwrap();
        assert_eq!(p.decide("fs_write", Risk::Write), Decision::Allow);
        assert_eq!(p.decide("system_info", Risk::Read), Decision::Deny);
        assert_eq!(p.decide("fs_read", Risk::Read), Decision::Allow);
        assert_eq!(p.decide("proc_run", Risk::Execute), Decision::Ask);
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(Policy::from_toml("[fs]\nwrite_root = []\n").is_err());
    }

    #[test]
    fn path_scopes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("ro/sub")).unwrap();
        std::fs::create_dir_all(dir.path().join("rw")).unwrap();
        let p = policy_for(dir.path());
        let s = |rel: &str| dir.path().join(rel).to_str().unwrap().to_string();

        assert!(p.check_read(&s("ro/sub")).is_ok());
        assert!(p.check_read(&s("rw")).is_ok());
        assert!(p.check_read(&s("ro/../..")).is_err());
        assert!(p.check_read("relative/path").is_err());

        assert!(p.check_write(&s("rw/new/file.txt")).is_ok());
        assert!(p.check_write(&s("ro/file.txt")).is_err());
        assert!(p.check_write(&s("rw/missing/../../ro/x")).is_err());

        std::os::unix::fs::symlink(dir.path().join("ro"), dir.path().join("rw/escape")).unwrap();
        assert!(p.check_write(&s("rw/escape/x")).is_err());
        std::os::unix::fs::symlink(dir.path().join("ro/nope"), dir.path().join("rw/dangling")).unwrap();
        assert!(p.check_write(&s("rw/dangling")).is_err());
    }
}
