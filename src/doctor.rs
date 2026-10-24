//! `orion doctor`: check what Orion stands on, in order (the policy file,
//! the env file and key, the models, voice, the sandbox, the privileged
//! helper and the daemon), say what is wrong in plain words, and offer to
//! fix each thing it can.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::chat::http::{clean_secret, non_empty_env, secret_env};
use crate::setup::{self, tilde, ClaudeAuth, Prompt, Verdict};
use crate::{envfile, Policy};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    /// Worth knowing; nothing is wrong.
    Note,
    Warn,
    Fail,
}

/// What a fix does, and how.
pub enum Fix {
    /// A change Orion makes itself; returns what it did.
    Edit(Box<dyn FnOnce() -> Result<String>>),
    /// A command to run as it is, in this terminal (sudo may ask).
    Run(Vec<String>),
}

pub struct Check {
    pub level: Level,
    pub what: String,
    /// The next line: why, or what to do by hand.
    pub hint: String,
    pub fix: Option<(String, Fix)>,
}

fn check(level: Level, what: impl Into<String>) -> Check {
    Check { level, what: what.into(), hint: String::new(), fix: None }
}

impl Check {
    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = hint.into();
        self
    }

    fn fix(mut self, label: impl Into<String>, fix: Fix) -> Self {
        self.fix = Some((label.into(), fix));
        self
    }
}

struct Doctor {
    ask: Option<Prompt>,
    worst: Level,
    fixed: usize,
}

impl Doctor {
    /// Print a finding, and offer its fix.
    fn report(&mut self, c: Check) -> Result<bool> {
        let mark = match c.level {
            Level::Ok => "ok",
            Level::Note => "--",
            Level::Warn => "!",
            Level::Fail => "!!",
        };
        println!("  {mark:2}  {}", c.what);
        if !c.hint.is_empty() {
            for line in c.hint.lines() {
                println!("      {line}");
            }
        }
        let Some((label, fix)) = c.fix else {
            self.worst = self.worst.max(c.level);
            return Ok(false);
        };
        let Some(ask) = self.ask.as_mut() else {
            println!("      fix: {label}  (run `orion doctor` in a terminal to apply it)");
            self.worst = self.worst.max(c.level);
            return Ok(false);
        };
        if !ask.yes(&format!("      Fix: {label}?"), true)? {
            self.worst = self.worst.max(c.level);
            return Ok(false);
        }
        let done = match fix {
            Fix::Edit(f) => f().map(|msg| println!("      done: {msg}")),
            Fix::Run(argv) => {
                println!("      $ {}", argv.join(" "));
                match Command::new(&argv[0]).args(&argv[1..]).status() {
                    Ok(s) if s.success() => Ok(()),
                    Ok(s) => Err(anyhow::anyhow!("it exited with {s}")),
                    Err(e) => Err(anyhow::anyhow!("couldn't run {}: {e}", argv[0])),
                }
            }
        };
        match done {
            Ok(()) => {
                self.fixed += 1;
                Ok(true)
            }
            Err(e) => {
                println!("      the fix failed: {e:#}");
                self.worst = self.worst.max(c.level);
                Ok(false)
            }
        }
    }
}

pub struct Options {
    /// Apply every fix without asking.
    pub yes: bool,
    /// Only report.
    pub no_fix: bool,
}

/// Run every check; exits 1 when something is still broken.
pub fn run(explicit: Option<&Path>, opts: Options) -> Result<()> {
    let ask = if opts.no_fix { None } else { Prompt::open(opts.yes) };
    let mut d = Doctor { ask, worst: Level::Ok, fixed: 0 };
    println!("Orion doctor\n");

    println!("policy");
    let policy = config(&mut d, explicit)?;
    println!("\nkeys");
    env_file(&mut d)?;
    println!("\nmodels");
    models(&mut d, &policy, explicit)?;
    println!("\nvoice");
    voice(&mut d, &policy)?;
    println!("\nsandbox");
    sandbox(&mut d, &policy)?;
    println!("\nroot access");
    helper(&mut d, &policy)?;
    println!("\ndaemon");
    daemon(&mut d, &policy)?;

    println!();
    let fixed = match d.fixed {
        0 => String::new(),
        1 => " (1 thing fixed)".into(),
        n => format!(" ({n} things fixed)"),
    };
    match d.worst {
        Level::Fail => {
            println!("Something above still needs fixing{fixed}.");
            std::process::exit(1);
        }
        Level::Warn => println!("Orion works; see the notes above{fixed}."),
        _ => println!("All good{fixed}."),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The policy file.

/// Which policy file Orion reads, and why.
fn config_path(explicit: Option<&Path>) -> (Option<PathBuf>, &'static str) {
    if let Some(p) = explicit {
        return (Some(p.to_path_buf()), "--config");
    }
    if let Some(p) = std::env::var_os("ORION_CONFIG").filter(|v| !v.is_empty()) {
        return (Some(PathBuf::from(p)), "ORION_CONFIG");
    }
    (Policy::default_config_path(), "default")
}

/// The policy as Orion will load it: checks, fixes, then loads it (the
/// built-in defaults when it can't).
fn config(d: &mut Doctor, explicit: Option<&Path>) -> Result<Policy> {
    let (path, why) = config_path(explicit);
    let Some(path) = path else {
        d.report(check(Level::Warn, "no home directory, so no policy file").hint("Orion runs on its built-in defaults"))?;
        return Policy::from_toml("");
    };
    if !path.exists() {
        let near = near_misses(&path);
        if why != "default" {
            let mut c = check(Level::Fail, format!("{why} names {}, which doesn't exist", tilde(&path)));
            c = c.hint(match near.first() {
                Some(n) => format!("did you mean {}? Fix {why} where it's set{}", tilde(n), where_set("ORION_CONFIG")),
                None => format!("fix {why}{}, or unset it to use ~/.config/orion/orion.toml", where_set("ORION_CONFIG")),
            });
            d.report(c)?;
            return Policy::from_toml("");
        }
        match near.first() {
            Some(found) => {
                let (from, to) = (found.clone(), path.clone());
                let c = check(Level::Fail, format!("no policy at {}, but there is {}", tilde(&path), tilde(found)))
                    .hint("a typo in the name? Orion only reads the one path")
                    .fix(
                        format!("move {} to {}", tilde(found), tilde(&path)),
                        Fix::Edit(Box::new(move || {
                            std::fs::create_dir_all(to.parent().expect("a directory"))?;
                            std::fs::rename(&from, &to).with_context(|| format!("moving {}", from.display()))?;
                            Ok(format!("moved it to {}", tilde(&to)))
                        })),
                    );
                if !d.report(c)? {
                    return Policy::from_toml("");
                }
                return config(d, explicit);
            }
            None => {
                d.report(
                    check(Level::Note, format!("no policy file at {}: Orion runs on its defaults", tilde(&path)))
                        .hint("`orion setup` writes one with your model in it"),
                )?;
                return Policy::from_toml("");
            }
        }
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let text = match cleaned_text(&text) {
        Some((clean, what)) => {
            let (p, old) = (path.clone(), text.clone());
            let c = check(Level::Fail, format!("{} has {what}", tilde(&path)))
                .hint("usually from pasting out of a web page or a chat; TOML wants plain quotes and spaces")
                .fix(
                    "replace them with plain ones",
                    Fix::Edit({
                        let clean = clean.clone();
                        Box::new(move || {
                            let bak = setup::replace(&p, Some(&old), &clean)?;
                            Ok(format!("cleaned it (the old one is {})", bak.map_or_else(String::new, |b| tilde(&b))))
                        })
                    }),
                );
            if d.report(c)? {
                // Look again, at the cleaned file.
                return config(d, explicit);
            }
            text
        }
        None => text,
    };
    match Policy::from_toml(&text) {
        Ok(_) => {
            d.report(check(Level::Ok, format!("{} loads{}", tilde(&path), if why == "default" { String::new() } else { format!(" (from {why})") })))?;
        }
        Err(e) => {
            let line = e.downcast_ref::<toml::de::Error>().and_then(|t| t.span()).map(|s| text[..s.start].matches('\n').count() + 1);
            let first = format!("{e}").lines().next().unwrap_or_default().to_string();
            let mut c = check(Level::Fail, format!("{} doesn't load: {first}", tilde(&path)))
                .hint(format!("{e}").lines().skip(1).collect::<Vec<_>>().join("\n"));
            if let (Some(n), true) = (line, format!("{e}").contains("unknown field")) {
                let (p, old) = (path.clone(), text.clone());
                c = c.fix(
                    format!("comment out line {n}, which Orion doesn't know"),
                    Fix::Edit(Box::new(move || {
                        let new: String = old
                            .lines()
                            .enumerate()
                            .map(|(i, l)| if i + 1 == n { format!("# {l}\n") } else { format!("{l}\n") })
                            .collect();
                        setup::replace(&p, Some(&old), &new)?;
                        Ok(format!("commented out line {n}"))
                    })),
                );
            } else {
                c.hint.push_str("\nfix that line, or run `orion setup`: it writes the model settings for you");
            }
            if d.report(c)? {
                return config(d, explicit);
            }
            return Policy::from_toml("");
        }
    }
    // Load it as Orion does, with the tuning applied.
    Ok(Policy::load(Some(&path)).map(|(p, _)| p).unwrap_or(Policy::from_toml(&text)?))
}

/// Characters a hand-edited file shouldn't have, and the text without them.
fn cleaned_text(text: &str) -> Option<(String, String)> {
    let mut found = Vec::new();
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{2033}' => {
                found.push("curly quotes");
                out.push('"');
            }
            '\u{2018}' | '\u{2019}' | '\u{2032}' => {
                found.push("curly quotes");
                out.push('\'');
            }
            '\u{a0}' | '\u{2007}' | '\u{202f}' => {
                found.push("non-breaking spaces");
                out.push(' ');
            }
            '\u{feff}' | '\u{200b}'..='\u{200f}' | '\u{2060}' => found.push("invisible characters"),
            c => out.push(c),
        }
    }
    found.dedup();
    let mut kinds: Vec<&str> = Vec::new();
    for f in found {
        if !kinds.contains(&f) {
            kinds.push(f);
        }
    }
    (!kinds.is_empty()).then(|| (out, kinds.join(" and ")))
}

/// Files that look like `wanted` misspelled: the same directory with the
/// name a little off, or the name in a directory a little off.
pub fn near_misses(wanted: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(name)) = (wanted.parent(), wanted.file_name().and_then(|n| n.to_str())) else { return Vec::new() };
    let stem = name.split('.').next().unwrap_or(name);
    let mut out = Vec::new();
    let close = |a: &str, b: &str| distance(&a.to_lowercase(), &b.to_lowercase()) <= 2;
    let files = |d: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(d).map(|r| r.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_file()).collect()).unwrap_or_default()
    };
    let looks_like = |p: &Path| {
        let n = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        n != name && (close(n, name) || (n.starts_with(stem) && (n.contains(".toml") || n.ends_with(".tml"))))
    };
    // ~/.config/orion/orion.tml, orion.toml.txt, Orion.toml
    out.extend(files(dir).into_iter().filter(|p| looks_like(p)));
    if let (Some(parent), Some(dname)) = (dir.parent(), dir.file_name().and_then(|n| n.to_str())) {
        // ~/.config/orin/orion.toml, ~/.config/Orion/orion.toml
        if let Ok(rd) = std::fs::read_dir(parent) {
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                let n = e.file_name().to_string_lossy().to_string();
                if p.is_dir() && n != dname && close(&n, dname) {
                    out.extend(files(&p).into_iter().filter(|f| f.file_name().and_then(|n| n.to_str()) == Some(name) || looks_like(f)));
                }
            }
        }
        // ~/.config/orion.toml
        let flat = parent.join(name);
        if flat.is_file() {
            out.push(flat);
        }
    }
    out.sort();
    out
}

/// Edit distance, for names.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for j in 0..b.len() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != b[j])).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

// ---------------------------------------------------------------------------
// The env file and the shell.

/// Shell start-up files that may export variables.
fn profiles() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return Vec::new() };
    let mut out: Vec<PathBuf> = [".bashrc", ".bash_profile", ".profile", ".zshrc", ".zprofile", ".zshenv", ".config/fish/config.fish"]
        .iter()
        .map(|f| home.join(f))
        .collect();
    if let Ok(rd) = std::fs::read_dir(home.join(".bashrc.d")) {
        out.extend(rd.filter_map(|e| e.ok()).map(|e| e.path()));
    }
    if let Ok(rd) = std::fs::read_dir(home.join(".config/environment.d")) {
        out.extend(rd.filter_map(|e| e.ok()).map(|e| e.path()));
    }
    out.into_iter().filter(|p| p.is_file()).collect()
}

/// Lines in the shell's start-up files that set `var`: (file, line, text).
pub fn profile_lines(var: &str) -> Vec<(PathBuf, usize, String)> {
    let mut out = Vec::new();
    for file in profiles() {
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        for (i, line) in text.lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with('#') {
                continue;
            }
            let t = t.strip_prefix("export ").or_else(|| t.strip_prefix("set -gx ")).or_else(|| t.strip_prefix("set -x ")).unwrap_or(t).trim_start();
            if t.strip_prefix(var).is_some_and(|rest| rest.starts_with('=') || rest.starts_with(' ')) {
                out.push((file.clone(), i + 1, line.to_string()));
            }
        }
    }
    out
}

/// Comment out the given lines, keeping each file as `.bak`.
pub fn comment_out(lines: &[(PathBuf, usize, String)]) -> Result<Vec<String>> {
    let mut done = Vec::new();
    let mut files: Vec<&PathBuf> = lines.iter().map(|(f, _, _)| f).collect();
    files.dedup();
    for file in files {
        let old = std::fs::read_to_string(file)?;
        let numbers: Vec<usize> = lines.iter().filter(|(f, _, _)| f == file).map(|(_, n, _)| *n).collect();
        let new: String = old
            .lines()
            .enumerate()
            .map(|(i, l)| if numbers.contains(&(i + 1)) { format!("# {l}  # commented out by orion: the key is in ~/.config/orion/env\n") } else { format!("{l}\n") })
            .collect();
        setup::replace(file, Some(&old), &new)?;
        done.push(format!("commented out {} line{} in {}", numbers.len(), if numbers.len() == 1 { "" } else { "s" }, tilde(file)));
    }
    Ok(done)
}

/// " (in ~/.bashrc:12)" when a start-up file sets `var`.
fn where_set(var: &str) -> String {
    let found: Vec<String> = profile_lines(var).iter().map(|(f, n, _)| format!("{}:{n}", tilde(f))).collect();
    if found.is_empty() { String::new() } else { format!(" (in {})", found.join(", ")) }
}

fn env_file(d: &mut Doctor) -> Result<()> {
    let Some(path) = envfile::path() else { return Ok(()) };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => {
            let shell_keys: Vec<&str> =
                ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "GEMINI_API_KEY"].into_iter().filter(|v| non_empty_env(v).is_some()).collect();
            let c = if shell_keys.is_empty() {
                check(Level::Note, format!("no env file at {}", tilde(&path))).hint("`orion setup` saves your key there")
            } else {
                check(Level::Note, format!("no env file at {}; {} only in this shell", tilde(&path), shell_keys.join(" and ")))
                    .hint("the background services can't see it; `orion setup` saves it where they can")
            };
            d.report(c)?;
            return Ok(());
        }
    };
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o600);
    if mode & 0o077 != 0 {
        let p = path.clone();
        d.report(
            check(Level::Warn, format!("{} is readable by others (mode {mode:o}), and it holds your keys", tilde(&path))).fix(
                "make it readable only by you (chmod 600)",
                Fix::Edit(Box::new(move || {
                    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
                    Ok("chmod 600".into())
                })),
            ),
        )?;
    }
    let entries = envfile::parse(&text);
    // Lines the services can't read as they are: `export`, keys with junk.
    let mut repaired = text.clone();
    let mut problems = Vec::new();
    for e in &entries {
        if e.export {
            problems.push(format!("line {}: `export` (the services want plain VAR=value)", e.line));
        }
        let secretish = ["KEY", "TOKEN", "SECRET"].iter().any(|w| e.name.contains(w));
        if secretish {
            match clean_secret(&e.value) {
                Ok(Some(clean)) if clean != e.value => {
                    problems.push(format!("line {}: {} has spaces or invisible characters", e.line, e.name));
                    repaired = envfile::with(&repaired, &e.name, &clean);
                    continue;
                }
                Ok(_) => {}
                Err(err) => {
                    d.report(check(Level::Fail, format!("{} line {}: {} {err}", tilde(&path), e.line, e.name)).hint("`orion setup` takes a fresh copy and checks it"))?;
                }
            }
        }
        if e.export {
            repaired = envfile::with(&repaired, &e.name, &e.value);
        }
    }
    if !problems.is_empty() {
        let p = path.clone();
        d.report(
            check(Level::Warn, format!("{}: {}", tilde(&path), problems.join("; "))).fix(
                "rewrite those lines",
                Fix::Edit(Box::new(move || {
                    envfile::write(&p, &repaired)?;
                    Ok(format!("rewrote {}", tilde(&p)))
                })),
            ),
        )?;
    }
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    d.report(check(Level::Ok, format!("{} sets {}", tilde(&path), if names.is_empty() { "nothing".into() } else { names.join(", ") })))?;
    // The shell's own copy wins; a different one is a stale key.
    for e in &entries {
        let Some(shell) = std::env::var(&e.name).ok().filter(|v| !v.is_empty()) else { continue };
        if envfile::loaded().contains(&e.name) {
            continue;
        }
        let same = clean_secret(&shell).ok().flatten() == clean_secret(&e.value).ok().flatten();
        if same {
            continue;
        }
        let found = profile_lines(&e.name);
        let mut c = check(Level::Warn, format!("this shell has a different {} from the env file's, and the shell's wins", e.name));
        if found.is_empty() {
            c = c.hint(format!("it was exported in this terminal or a parent; open a new terminal, or run: unset {}", e.name));
        } else {
            let at: Vec<String> = found.iter().map(|(f, n, _)| format!("{}:{n}", tilde(f))).collect();
            c = c.hint(format!("set in {}; after the fix, open a new terminal (or: unset {})", at.join(", "), e.name)).fix(
                format!("comment out {}", at.join(", ")),
                Fix::Edit(Box::new(move || Ok(comment_out(&found)?.join("; ")))),
            );
        }
        d.report(c)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The models.

fn models(d: &mut Doctor, policy: &Policy, explicit: Option<&Path>) -> Result<()> {
    use crate::chat::Mode;
    let (want_claude, want_local) = match policy.chat.mode {
        Mode::Claude => (true, false),
        Mode::Local => (false, true),
        Mode::Auto => (true, true),
    };
    let either = policy.chat.mode == Mode::Auto;
    let missing = if either { Level::Warn } else { Level::Fail };
    let mut working = 0;
    if want_claude {
        working += usize::from(claude_check(d, policy, explicit, missing)?);
    }
    if want_local {
        working += usize::from(local_check(d, policy, explicit, missing)?);
    }
    if either && working == 0 {
        d.report(check(Level::Fail, "no model answers, so the chat can't start").hint("`orion setup` connects one and checks it"))?;
    }
    Ok(())
}

/// The policy file to change for a fix.
fn fix_path(explicit: Option<&Path>) -> Option<PathBuf> {
    config_path(explicit).0
}

fn model_fix(explicit: Option<&Path>, key: &'static str, model: String) -> Option<(String, Fix)> {
    let path = fix_path(explicit)?;
    let label = format!("set {key} = \"{model}\"");
    Some((
        label,
        Fix::Edit(Box::new(move || {
            setup::edit_config(&path, &[setup::set(key, model.as_str())])?;
            Ok(format!("{key} is now \"{model}\""))
        })),
    ))
}

fn claude_check(d: &mut Doctor, policy: &Policy, explicit: Option<&Path>, missing: Level) -> Result<bool> {
    let auth = match (secret_env("ANTHROPIC_API_KEY"), secret_env("ANTHROPIC_AUTH_TOKEN")) {
        (Err(e), _) | (Ok(None), Err(e)) => {
            d.report(check(Level::Fail, format!("Claude: {e}")).hint("`orion setup` takes a fresh copy and checks it"))?;
            return Ok(false);
        }
        (Ok(Some(k)), _) => ClaudeAuth::Key(k),
        (Ok(None), Ok(Some(t))) => ClaudeAuth::Token(t),
        (Ok(None), Ok(None)) => {
            d.report(check(missing, "Claude: no key (ANTHROPIC_API_KEY)").hint("`orion setup` saves one and checks it"))?;
            return Ok(false);
        }
    };
    let base = non_empty_env("ANTHROPIC_BASE_URL").unwrap_or_else(|| policy.chat.claude.base_url.clone());
    let http = setup::claude_http(&base, &auth);
    let model = &policy.chat.claude.model;
    let models = match setup::claude_models(&http) {
        Ok(m) => m,
        Err(Verdict::Rejected(m)) => {
            d.report(check(Level::Fail, "Claude: the key is turned down").hint(format!("{m}\n`orion setup` takes a new one and checks it")))?;
            return Ok(false);
        }
        Err(Verdict::Limited(m)) => {
            d.report(check(Level::Warn, "Claude: the key works, but is rate limited right now").hint(m))?;
            return Ok(true);
        }
        Err(v) => {
            d.report(check(Level::Fail, format!("Claude: can't reach {base}")).hint(v.to_string()))?;
            return Ok(false);
        }
    };
    if models.is_empty() || models.contains(model) {
        d.report(check(Level::Ok, format!("Claude: the key works, and {model} is offered")))?;
        return Ok(true);
    }
    let mut c = check(Level::Fail, format!("Claude: the key works, but there's no model {model} (retired, or misspelled)"));
    c.fix = setup::suggest(&models, model).and_then(|s| model_fix(explicit, "chat.claude.model", s));
    d.report(c)
}

fn local_check(d: &mut Doctor, policy: &Policy, explicit: Option<&Path>, missing: Level) -> Result<bool> {
    let cfg = &policy.chat.local;
    let gemini = cfg.base_url.contains("googleapis");
    let name = if gemini { "Gemini".to_string() } else { format!("local model at {}", cfg.base_url) };
    let key = if cfg.api_key_env.is_empty() {
        None
    } else {
        match secret_env(&cfg.api_key_env) {
            Ok(Some(k)) => Some(k),
            Ok(None) => {
                d.report(check(missing, format!("{name}: {} isn't set", cfg.api_key_env)).hint(format!(
                    "`orion setup` saves it in ~/.config/orion/env{}",
                    if gemini { "" } else { ", or unset chat.local.api_key_env" }
                )))?;
                return Ok(false);
            }
            Err(e) => {
                d.report(check(Level::Fail, format!("{name}: {e}")).hint("`orion setup` takes a fresh copy and checks it"))?;
                return Ok(false);
            }
        }
    };
    let http = setup::openai_http(&cfg.base_url, key.as_deref());
    let models = match setup::openai_models(&http, Duration::from_secs(10)) {
        Ok(m) => m,
        Err(Verdict::Rejected(m)) => {
            d.report(check(Level::Fail, format!("{name}: the key is turned down")).hint(format!("{m}\n`orion setup` takes a new one and checks it")))?;
            return Ok(false);
        }
        Err(Verdict::Limited(m)) => {
            d.report(check(Level::Warn, format!("{name}: rate limited right now")).hint(m))?;
            return Ok(true);
        }
        Err(v) => {
            let hint = if gemini || !crate::chat::local::is_on_machine(&cfg.base_url) {
                v.to_string()
            } else {
                format!("{v}\nstart the server (Ollama: systemctl status ollama), or point chat.local.base_url at yours")
            };
            d.report(check(missing, format!("{name}: no answer")).hint(hint))?;
            return Ok(false);
        }
    };
    let offered = if gemini { setup::gemini_chat_models(&models) } else { models.clone() };
    if cfg.model.is_empty() {
        return Ok(match models.first() {
            Some(first) => {
                d.report(check(Level::Ok, format!("{name}: answers; no model set, so it uses the first, {first}")))?;
                true
            }
            None => {
                d.report(check(missing, format!("{name}: answers, but has no models")).hint("for Ollama: ollama pull qwen3:8b"))?;
                false
            }
        });
    }
    let wanted = cfg.model.strip_prefix("models/").unwrap_or(&cfg.model);
    if models.iter().any(|m| m == wanted) {
        if gemini && !offered.iter().any(|m| m == wanted) {
            let mut c = check(Level::Fail, format!("{name}: {wanted} makes speech or pictures, and can't chat"));
            c.fix = setup::suggest(&offered, "gemini-flash").and_then(|s| model_fix(explicit, "chat.local.model", s));
            return d.report(c);
        }
        d.report(check(Level::Ok, format!("{name}: answers, and {wanted} is offered")))?;
        return Ok(true);
    }
    let mut c = check(Level::Fail, format!("{name}: there's no model {wanted} (retired, or misspelled)"));
    if offered.is_empty() {
        c = c.hint(if gemini { "the key lists no chat models" } else { "the server has no models; for Ollama: ollama pull qwen3:8b" });
    } else {
        c.fix = setup::suggest(&offered, wanted).and_then(|s| model_fix(explicit, "chat.local.model", s));
    }
    d.report(c)
}

// ---------------------------------------------------------------------------
// Voice.

/// The package manager's install command, as root.
fn install_command(packages: &[(&str, &str)]) -> Option<Vec<String>> {
    let (manager, args): (&str, &[&str]) = [
        ("dnf", &["install", "-y"][..]),
        ("apt-get", &["install", "-y"][..]),
        ("zypper", &["--non-interactive", "install"][..]),
        ("pacman", &["-S", "--needed", "--noconfirm"][..]),
    ]
    .into_iter()
    .find(|(m, _)| on_path(m).is_some())?;
    let pkg = packages.iter().find(|(m, _)| *m == manager || m.is_empty())?.1;
    let mut argv: Vec<String> = Vec::new();
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        argv.push("sudo".into());
    }
    argv.push(manager.into());
    argv.extend(args.iter().map(|a| a.to_string()));
    argv.push(pkg.into());
    Some(argv)
}

fn on_path(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = crate::policy::expand_home(Path::new(name));
        return p.is_file().then_some(p);
    }
    std::env::var_os("PATH")?.to_str()?.split(':').map(|d| PathBuf::from(d).join(name)).find(|p| p.is_file())
}

fn voice(d: &mut Doctor, policy: &Policy) -> Result<()> {
    let v = &policy.voice;
    if !v.enabled {
        d.report(check(Level::Note, "voice is off (voice.enabled = false)"))?;
        return Ok(());
    }
    // The microphone.
    match crate::voice::record::Recorder::detect(v) {
        Err(e) => {
            let mut c = check(Level::Warn, "microphone: no recorder").hint(format!("{e:#}"));
            if let Some(argv) = install_command(&[("pacman", "libpulse"), ("", "pulseaudio-utils")]) {
                c = c.fix(format!("install one: {}", argv.join(" ")), Fix::Run(argv));
            }
            d.report(c)?;
        }
        Ok(rec) => {
            let c = match listen(&rec, Duration::from_millis(1500)) {
                Ok((0, _)) => check(Level::Warn, format!("microphone: {} started, but gave no sound", rec.describe()))
                    .hint("is a microphone plugged in and chosen as the input? (pavucontrol, or the desktop's sound settings)"),
                Ok((_, 0)) => check(Level::Warn, format!("microphone: {} gives only silence", rec.describe()))
                    .hint("the input may be muted, or turned all the way down"),
                Ok((_, peak)) => check(Level::Ok, format!("microphone: {} hears sound (peak {peak})", rec.describe())),
                Err(e) => check(Level::Warn, format!("microphone: {} fails", rec.describe())).hint(format!("{e:#}")),
            };
            d.report(c)?;
        }
    }
    // The transcriber.
    match crate::voice::transcribe::Transcriber::from_config(v) {
        Err(e) => {
            d.report(check(Level::Warn, "transcriber: not set up").hint(format!("{e:#}")))?;
        }
        Ok(crate::voice::transcribe::Transcriber::Command(cmd)) => {
            let program = cmd.split_whitespace().next().unwrap_or_default().to_string();
            d.report(match on_path(&program) {
                Some(_) => check(Level::Ok, format!("transcriber: {program}")),
                None => check(Level::Warn, format!("transcriber: {program} isn't installed (voice.transcribe_command)")),
            })?;
        }
        Ok(t) => {
            let key = if v.api_key_env.is_empty() { None } else { secret_env(&v.api_key_env).ok().flatten() };
            let http = setup::openai_http(&v.transcribe_url, key.as_deref());
            let c = match setup::openai_models(&http, Duration::from_secs(5)) {
                Ok(_) | Err(Verdict::NoModel(_)) | Err(Verdict::Limited(_)) => check(Level::Ok, format!("transcriber: {} answers", t.describe())),
                Err(Verdict::Rejected(m)) => check(Level::Warn, format!("transcriber: {} turns the key down", v.transcribe_url)).hint(m),
                Err(Verdict::Failed(m)) => check(Level::Warn, format!("transcriber: nothing answers at {}", v.transcribe_url)).hint(format!(
                    "{m}\nstart a speech-to-text server there (speaches, faster-whisper-server), or set voice.transcribe_command"
                )),
            };
            d.report(c)?;
        }
    }
    // The wake word.
    if v.wake_command.is_empty() {
        let c = if v.wake {
            check(Level::Note, "wake word: on, by transcribing what's said (no wake_command)")
                .hint("contrib/orion-wake spots the word on this machine instead, and lighter")
        } else {
            check(Level::Note, "wake word: off (voice.wake = false)")
        };
        d.report(c)?;
    } else {
        let mut words = v.wake_command.split_whitespace();
        let program = words.next().unwrap_or_default().to_string();
        match on_path(&program) {
            None => {
                d.report(
                    check(Level::Warn, format!("wake word: {program} isn't there (voice.wake_command)"))
                        .hint("install it from a checkout: install -m 755 contrib/orion-wake ~/.local/bin/orion-wake"),
                )?;
            }
            Some(path) if program.ends_with("orion-wake") || path.ends_with("orion-wake") => {
                let engine = if v.wake_command.contains("openwakeword") { "openwakeword" } else { "vosk" };
                let ok = Command::new("python3")
                    .args(["-c", &format!("import {engine}")])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
                let mut c = if ok {
                    check(Level::Ok, format!("wake word: orion-wake, with {engine}"))
                } else {
                    check(Level::Warn, format!("wake word: orion-wake needs the Python module {engine}"))
                };
                if !ok {
                    let pkgs = if engine == "vosk" { "vosk" } else { "openwakeword onnxruntime" };
                    let mut argv: Vec<String> = vec!["python3".into(), "-m".into(), "pip".into(), "install".into(), "--user".into()];
                    argv.extend(pkgs.split(' ').map(String::from));
                    c = c.fix(format!("install it: {}", argv.join(" ")), Fix::Run(argv));
                }
                d.report(c)?;
            }
            Some(_) => {
                d.report(check(Level::Ok, format!("wake word: {program}")))?;
            }
        }
    }
    Ok(())
}

/// Record for `time`: the bytes the recorder gave, and the loudest sample.
fn listen(rec: &crate::voice::record::Recorder, time: Duration) -> Result<(usize, u16)> {
    let mut child = rec.spawn()?;
    let mut out = child.stdout.take().context("no output from the recorder")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 8192];
        loop {
            match out.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let start = Instant::now();
    let (mut bytes, mut peak, mut odd) = (0usize, 0u16, None::<u8>);
    // A recorder can take a moment to start, so allow for that.
    let deadline = start + time + Duration::from_secs(2);
    while Instant::now() < deadline && !(bytes > 0 && start.elapsed() >= time) {
        let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) else { continue };
        bytes += chunk.len();
        let mut data = chunk;
        if let Some(b) = odd.take() {
            data.insert(0, b);
        }
        if data.len() % 2 == 1 {
            odd = data.pop();
        }
        for s in data.as_chunks::<2>().0 {
            peak = peak.max(i16::from_le_bytes([s[0], s[1]]).unsigned_abs());
        }
    }
    // The recorder runs in a process group of its own: stop all of it.
    // SAFETY: signalling a process group we started.
    unsafe { libc::kill(-(child.id() as i32), libc::SIGTERM) };
    let _ = child.wait();
    // Background hiss isn't sound.
    Ok((bytes, if peak < 8 { 0 } else { peak }))
}

// ---------------------------------------------------------------------------
// The sandbox.

fn sandbox(d: &mut Doctor, policy: &Policy) -> Result<()> {
    use crate::sandbox::{backend, Backend};
    let bwrap = on_path("bwrap").is_some();
    let c = match backend(&policy.sandbox) {
        Ok(Backend::Bubblewrap) => check(Level::Ok, crate::sandbox::describe(policy)),
        Ok(Backend::Off) => check(Level::Warn, "off (sandbox.mode = \"off\"): programs the AI runs aren't confined"),
        Ok(_) | Err(_) if !bwrap => {
            let mut c = check(
                if backend(&policy.sandbox).is_err() { Level::Fail } else { Level::Warn },
                format!("{}; bubblewrap isn't installed", crate::sandbox::describe(policy)),
            )
            .hint("with bubblewrap, programs get no network, no GPU and no private paths");
            if let Some(argv) = install_command(&[("", "bubblewrap")]) {
                c = c.fix(format!("install it: {}", argv.join(" ")), Fix::Run(argv));
            }
            c
        }
        Ok(_) => check(Level::Warn, format!("{}; bubblewrap is installed but can't make a sandbox here", crate::sandbox::describe(policy))).hint(
            "unprivileged user namespaces are probably restricted: check sysctl kernel.unprivileged_userns_clone\n\
             and kernel.apparmor_restrict_unprivileged_userns",
        ),
        Err(e) => check(Level::Fail, format!("{e:#}")),
    };
    d.report(c)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The privileged helper.

/// The groups this process has: what the kernel checks, which only change
/// at a new login.
fn process_groups() -> Vec<u32> {
    // SAFETY: a first call for the count, then one into a buffer that size.
    let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if n <= 0 {
        return Vec::new();
    }
    let mut groups = vec![0 as libc::gid_t; n as usize];
    let n = unsafe { libc::getgroups(n, groups.as_mut_ptr()) };
    groups.truncate(n.max(0) as usize);
    // SAFETY: getegid has no preconditions.
    groups.push(unsafe { libc::getegid() });
    groups
}

fn helper(d: &mut Doctor, policy: &Policy) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        d.report(check(Level::Ok, "Orion runs as root"))?;
        return Ok(());
    }
    let group = &policy.helper.group;
    let socket = PathBuf::from(&policy.helper.socket);
    let installed = Path::new("/usr/local/bin/orion").exists() || socket.exists();
    let Some(gid) = crate::helper::group_gid(group) else {
        let c = if installed {
            check(Level::Warn, format!("the helper's group, {group}, doesn't exist")).fix(
                format!("create it: sudo groupadd --system {group}"),
                Fix::Run(vec!["sudo".into(), "groupadd".into(), "--system".into(), group.clone()]),
            )
        } else {
            check(Level::Note, format!("the privileged helper isn't installed; root work goes through {}", crate::tools::root_access(policy)))
                .hint("packaging/bootstrap.sh installs it, or see docs/install.md, \"The privileged helper\"")
        };
        d.report(c)?;
        return Ok(());
    };
    let user = std::env::var("USER").ok().filter(|u| !u.is_empty()).or_else(login_name).unwrap_or_default();
    // SAFETY: getgid has no preconditions.
    let primary = unsafe { libc::getgid() };
    let listed = policy.helper.users.contains(&user);
    let member = crate::helper::user_groups(&user, primary).contains(&gid);
    let this_login = process_groups().contains(&gid);
    let mut usable = listed;
    if member && !this_login {
        d.report(
            check(Level::Warn, format!("you're in the {group} group, but this login started before you were added"))
                .hint(format!("log out and back in (or restart) once; for just this terminal: newgrp {group}")),
        )?;
    } else if !member && !listed {
        let c = check(Level::Warn, format!("you're not in the {group} group, so the helper won't answer you")).fix(
            format!("add yourself: sudo usermod -aG {group} {user} (then log in again)"),
            Fix::Run(vec!["sudo".into(), "usermod".into(), "-aG".into(), group.clone(), user.clone()]),
        );
        d.report(c)?;
    } else {
        usable = true;
    }
    match std::os::unix::net::UnixStream::connect(&socket) {
        Ok(_) => {
            d.report(check(Level::Ok, crate::tools::root_access(policy)))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && !usable => {}
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            d.report(check(Level::Warn, format!("the helper at {} turns you away: {e}", socket.display())))?;
        }
        Err(_) => {
            d.report(check(Level::Warn, format!("the helper isn't running at {}", socket.display())).fix(
                "start it: sudo systemctl enable --now orion-helper.socket",
                Fix::Run(["sudo", "systemctl", "enable", "--now", "orion-helper.socket"].map(String::from).to_vec()),
            ))?;
        }
    }
    Ok(())
}

fn login_name() -> Option<String> {
    // SAFETY: getlogin returns a static string or null.
    let p = unsafe { libc::getlogin() };
    // SAFETY: non-null, so a NUL-terminated string.
    (!p.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().to_string())
}

// ---------------------------------------------------------------------------
// The daemon.

fn daemon(d: &mut Doctor, policy: &Policy) -> Result<()> {
    let st = match crate::daemon::status(&policy.daemon) {
        Ok(st) => st,
        Err(_) => {
            d.report(
                check(Level::Note, "the background service isn't running")
                    .hint("optional: it shares one policy and audit log among agents (systemctl --user enable --now orion.socket)"),
            )?;
            return Ok(());
        }
    };
    let up = st["uptime_secs"].as_u64().unwrap_or(0);
    // Started before the env file last changed: it has the old keys.
    let changed = envfile::path()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|since| since.as_secs() < up);
    if changed {
        d.report(check(Level::Warn, "the background service started before the env file last changed, so it has the old keys").fix(
            "restart it: systemctl --user restart orion.service",
            Fix::Run(["systemctl", "--user", "restart", "orion.service"].map(String::from).to_vec()),
        ))?;
    } else {
        d.report(check(Level::Ok, format!("running, pid {}, up {}h {}m", st["pid"], up / 3600, up / 60 % 60)))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_misspelled_policy_files() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".config");
        std::fs::create_dir_all(config.join("orin")).unwrap();
        std::fs::create_dir_all(config.join("orion")).unwrap();
        std::fs::create_dir_all(config.join("other")).unwrap();
        std::fs::write(config.join("orin/orion.toml"), "").unwrap();
        std::fs::write(config.join("orion/orion.tml"), "").unwrap();
        std::fs::write(config.join("orion/orion.toml.txt"), "").unwrap();
        std::fs::write(config.join("orion/env"), "").unwrap();
        std::fs::write(config.join("other/orion.toml"), "").unwrap();
        let found = near_misses(&config.join("orion/orion.toml"));
        let names: Vec<String> = found.iter().map(|p| p.strip_prefix(&config).unwrap().display().to_string()).collect();
        assert_eq!(names, ["orin/orion.toml", "orion/orion.tml", "orion/orion.toml.txt"]);
        // ~/.config/orion.toml, when the directory is missing altogether.
        let bare = tempfile::tempdir().unwrap();
        std::fs::write(bare.path().join("orion.toml"), "").unwrap();
        assert_eq!(near_misses(&bare.path().join("orion/orion.toml")), [bare.path().join("orion.toml")]);
        assert_eq!(distance("orion", "orin"), 1);
        assert_eq!(distance("orion", "Orion".to_lowercase().as_str()), 0);
    }

    #[test]
    fn straightens_pasted_quotes() {
        let (clean, what) = cleaned_text("[chat]\nprovider = \u{201c}claude\u{201d}\u{a0}\n\u{feff}").unwrap();
        assert_eq!(clean, "[chat]\nprovider = \"claude\" \n");
        assert_eq!(what, "curly quotes and non-breaking spaces and invisible characters");
        assert!(Policy::from_toml(&clean).is_ok());
        assert!(cleaned_text("[chat]\nprovider = \"claude\"\n").is_none());
    }

    #[test]
    fn finds_and_comments_out_exports() {
        let home = tempfile::tempdir().unwrap();
        let rc = home.path().join(".bashrc");
        std::fs::write(&rc, "alias l=ls\nexport ORION_TEST_K=old\n# export ORION_TEST_K=older\nORION_TEST_KX=1\n").unwrap();
        let lines = vec![(rc.clone(), 2, "export ORION_TEST_K=old".to_string())];
        comment_out(&lines).unwrap();
        let text = std::fs::read_to_string(&rc).unwrap();
        assert!(text.starts_with("alias l=ls\n# export ORION_TEST_K=old  # commented out"), "{text}");
        assert!(text.ends_with("ORION_TEST_KX=1\n"));
        assert!(home.path().join(".bashrc.bak").exists());
    }

    #[test]
    fn names_the_unknown_key_and_its_line() {
        let text = "[chat]\nprovider = \"claude\"\nmodle = \"x\"\n";
        let e = Policy::from_toml(text).unwrap_err();
        let span = e.downcast_ref::<toml::de::Error>().and_then(|t| t.span()).unwrap();
        assert_eq!(text[..span.start].matches('\n').count() + 1, 3);
        assert!(format!("{e}").contains("unknown field"));
    }
}
