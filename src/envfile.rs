//! `~/.config/orion/env`: the variables Orion needs, such as
//! `ANTHROPIC_API_KEY`, one `VAR=value` per line. The systemd units read it
//! with `EnvironmentFile=`, and every `orion` command reads it at start, so
//! a key saved there works at once, without a new shell or a login.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};

/// `$XDG_CONFIG_HOME/orion/env`.
pub fn path() -> Option<PathBuf> {
    crate::Policy::default_config_path().map(|p| p.with_file_name("env"))
}

/// One `VAR=value` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// 1-based.
    pub line: usize,
    pub name: String,
    pub value: String,
    /// The line began with `export `, which systemd doesn't take.
    pub export: bool,
}

/// The assignments in `text`; blank lines, comments and lines that assign
/// nothing are skipped. Values may be quoted, as in a shell.
pub fn parse(text: &str) -> Vec<Entry> {
    text.lines()
        .enumerate()
        .filter_map(|(i, raw)| {
            let line = raw.trim_start_matches('\u{feff}').trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (export, line) = match line.strip_prefix("export ") {
                Some(rest) => (true, rest.trim_start()),
                None => (false, line),
            };
            let (name, value) = line.split_once('=')?;
            let name = name.trim();
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return None;
            }
            Some(Entry { line: i + 1, name: name.to_string(), value: unquote(value.trim()).to_string(), export })
        })
        .collect()
}

fn unquote(v: &str) -> &str {
    for q in ['"', '\''] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            return &v[1..v.len() - 1];
        }
    }
    v
}

static LOADED: OnceLock<Vec<String>> = OnceLock::new();

/// Set each variable in the env file that the environment doesn't already
/// have: what the shell exports wins. Call once, at start, before threads.
pub fn load() {
    let mut names = Vec::new();
    if let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) {
        for e in parse(&text) {
            if std::env::var_os(&e.name).is_none_or(|v| v.is_empty()) && !e.value.is_empty() {
                std::env::set_var(&e.name, &e.value);
                names.push(e.name);
            }
        }
    }
    let _ = LOADED.set(names);
}

/// The variables [`load`] set, which programs Orion runs for the AI are not
/// given.
pub fn loaded() -> &'static [String] {
    LOADED.get().map_or(&[], Vec::as_slice)
}

/// Set `name` to `value` in the file at `path`, replacing any line that set
/// it before and keeping the others. The file is readable only by you.
pub fn set(path: &Path, name: &str, value: &str) -> Result<()> {
    let old = std::fs::read_to_string(path).unwrap_or_default();
    write(path, &with(&old, name, value))
}

/// `text` with `name` set to `value`.
pub fn with(text: &str, name: &str, value: &str) -> String {
    let set_here: Vec<usize> = parse(text).into_iter().filter(|e| e.name == name).map(|e| e.line).collect();
    let mut out = String::new();
    let mut done = false;
    for (i, line) in text.lines().enumerate() {
        if set_here.contains(&(i + 1)) {
            if !done {
                out.push_str(&format!("{name}={value}\n"));
                done = true;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !done {
        out.push_str(&format!("{name}={value}\n"));
    }
    out
}

/// Replace the file at `path` with `text`, mode 600, all at once.
pub fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_what_people_write() {
        let text = "\u{feff}# keys\nANTHROPIC_API_KEY=sk-ant-1\n\nexport GEMINI_API_KEY=\"AIza 2\"\nX='y'\nnot a line\n=v\n";
        let got = parse(text);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], Entry { line: 2, name: "ANTHROPIC_API_KEY".into(), value: "sk-ant-1".into(), export: false });
        assert_eq!((got[1].line, got[1].name.as_str(), got[1].value.as_str(), got[1].export), (4, "GEMINI_API_KEY", "AIza 2", true));
        assert_eq!(got[2].value, "y");
    }

    #[test]
    fn replaces_a_variable_and_keeps_the_rest() {
        let text = "# mine\nA=1\nB=2\nexport A=3\n";
        assert_eq!(with(text, "A", "new"), "# mine\nA=new\nB=2\n");
        assert_eq!(with(text, "C", "x"), "# mine\nA=1\nB=2\nexport A=3\nC=x\n");
        assert_eq!(with("", "C", "x"), "C=x\n");
    }

    #[test]
    fn writes_it_private() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("orion/env");
        set(&p, "K", "v1").unwrap();
        set(&p, "K", "v2").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "K=v2\n");
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
