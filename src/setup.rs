//! `orion setup`: pick the model, paste its key, and Orion checks the key
//! with a live call and writes `orion.toml` and the env file itself, so
//! nobody hand-edits TOML or hunts for invisible characters in a key.

use std::fs::File;
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::chat::http::{clean_secret, Http, StatusError};
use crate::{envfile, Policy};

pub const GEMINI_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai";
pub const CLAUDE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// The usual local servers: Ollama, llama-server, vLLM, LM Studio.
const LOCAL_URLS: [&str; 4] =
    ["http://127.0.0.1:11434/v1", "http://127.0.0.1:8080/v1", "http://127.0.0.1:8000/v1", "http://127.0.0.1:1234/v1"];

/// What `orion setup` was told on the command line.
#[derive(Debug, Default)]
pub struct Options {
    /// claude, gemini or local; asked when unset.
    pub provider: Option<String>,
    pub model: Option<String>,
    /// For a local server: its OpenAI-compatible URL.
    pub base_url: Option<String>,
    /// Take the default answer to every question, and a key already in the
    /// environment without asking.
    pub yes: bool,
}

// ---------------------------------------------------------------------------
// Questions in the terminal.

/// Questions and answers on the terminal: stdin when it is one, else
/// `/dev/tty` (as under `curl … | sh`).
pub struct Prompt {
    input: Box<dyn BufRead>,
    /// The terminal, to turn echo off for keys.
    tty: Option<File>,
    stdin_tty: bool,
    pub assume_yes: bool,
}

impl Prompt {
    /// `None` when there is no terminal to ask on, unless `assume_yes`
    /// means nothing need be asked.
    pub fn open(assume_yes: bool) -> Option<Self> {
        if std::io::stdin().is_terminal() {
            let input = Box::new(BufReader::new(std::io::stdin()));
            return Some(Self { input, tty: None, stdin_tty: true, assume_yes });
        }
        let Ok(tty) = File::options().read(true).write(true).open("/dev/tty") else {
            return assume_yes.then(|| Self { input: Box::new(std::io::empty()), tty: None, stdin_tty: false, assume_yes });
        };
        let input = Box::new(BufReader::new(tty.try_clone().ok()?));
        Some(Self { input, tty: Some(tty), stdin_tty: false, assume_yes })
    }

    /// Answers read from `text`, one per line, for tests.
    pub fn scripted(text: &str) -> Self {
        Self { input: Box::new(std::io::Cursor::new(text.to_string().into_bytes())), tty: None, stdin_tty: false, assume_yes: false }
    }

    fn read_line(&mut self) -> Result<String> {
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            bail!("no answer (end of input)");
        }
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }

    pub fn line(&mut self, question: &str) -> Result<String> {
        print!("{question}");
        std::io::stdout().flush()?;
        Ok(self.read_line()?.trim().to_string())
    }

    /// A yes or no; Enter takes `default`.
    pub fn yes(&mut self, question: &str, default: bool) -> Result<bool> {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        if self.assume_yes {
            println!("{question} {hint} {}", if default { "y" } else { "n" });
            return Ok(default);
        }
        loop {
            match self.line(&format!("{question} {hint} "))?.to_ascii_lowercase().as_str() {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => println!("  y or n, please"),
            }
        }
    }

    /// One of `options`, by number; Enter takes `default` (0-based).
    pub fn choose(&mut self, question: &str, options: &[String], default: usize) -> Result<usize> {
        println!("{question}");
        for (i, o) in options.iter().enumerate() {
            println!("  {}) {o}", i + 1);
        }
        if self.assume_yes {
            println!("Choose [{}]: {}", default + 1, default + 1);
            return Ok(default);
        }
        loop {
            let answer = self.line(&format!("Choose [{}]: ", default + 1))?;
            if answer.is_empty() {
                return Ok(default);
            }
            match answer.parse::<usize>() {
                Ok(n) if (1..=options.len()).contains(&n) => return Ok(n - 1),
                _ => println!("  a number from 1 to {}, please", options.len()),
            }
        }
    }

    /// A line typed without echo, such as a key.
    pub fn secret(&mut self, question: &str) -> Result<String> {
        print!("{question}");
        std::io::stdout().flush()?;
        let fd = match (&self.tty, self.stdin_tty) {
            (Some(t), _) => Some(t.as_raw_fd()),
            (None, true) => Some(0),
            _ => None,
        };
        let saved = fd.and_then(|fd| {
            // SAFETY: a zeroed termios is a valid out-parameter; fd is open.
            let mut t: libc::termios = unsafe { std::mem::zeroed() };
            (unsafe { libc::tcgetattr(fd, &mut t) } == 0).then(|| {
                let mut quiet = t;
                quiet.c_lflag &= !libc::ECHO;
                quiet.c_lflag |= libc::ECHONL;
                // SAFETY: as above.
                unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) };
                (fd, t)
            })
        });
        let line = self.read_line();
        if let Some((fd, t)) = saved {
            // SAFETY: restores the settings read above.
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) };
        }
        line
    }
}

// ---------------------------------------------------------------------------
// Talking to the model's API.

/// How a check of a key or model came out.
#[derive(Debug)]
pub enum Verdict {
    /// The key was turned down.
    Rejected(String),
    /// There's no such model (retired, or misspelled).
    NoModel(String),
    /// Too many requests: the key works, but its quota is used up for now.
    Limited(String),
    /// Anything else: no network, the server is down.
    Failed(String),
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Verdict::Rejected(m) | Verdict::NoModel(m) | Verdict::Limited(m) | Verdict::Failed(m) => f.write_str(m),
        }
    }
}

pub fn verdict(e: anyhow::Error) -> Verdict {
    let msg = format!("{e:#}");
    match e.downcast_ref::<StatusError>().map(|s| s.code) {
        Some(401 | 403) => Verdict::Rejected(msg),
        Some(404) => Verdict::NoModel(msg),
        Some(400) if msg.to_ascii_lowercase().contains("api key") => Verdict::Rejected(msg),
        Some(400) if msg.to_ascii_lowercase().contains("model") => Verdict::NoModel(msg),
        Some(429) => Verdict::Limited(msg),
        _ => Verdict::Failed(msg),
    }
}

/// How to sign in to the Claude API.
#[derive(Clone)]
pub enum ClaudeAuth {
    Key(String),
    Token(String),
}

pub fn claude_http(base: &str, auth: &ClaudeAuth) -> Http {
    let mut headers = vec![("anthropic-version".to_string(), ANTHROPIC_VERSION.to_string())];
    match auth {
        ClaudeAuth::Key(k) => headers.push(("x-api-key".into(), k.clone())),
        ClaudeAuth::Token(t) => {
            headers.push(("authorization".into(), format!("Bearer {t}")));
            headers.push(("anthropic-beta".into(), "oauth-2025-04-20".into()));
        }
    }
    Http::new(base, headers)
}

/// The models the key may use, newest first.
pub fn claude_models(http: &Http) -> Result<Vec<String>, Verdict> {
    let v = http.get_json_within("/v1/models?limit=1000", Duration::from_secs(20)).map_err(verdict)?;
    Ok(ids(&v))
}

/// Whether `model` answers, with the smallest request there is.
pub fn claude_try(http: &Http, model: &str) -> Result<(), Verdict> {
    let body = json!({ "model": model, "max_tokens": 8, "messages": [{ "role": "user", "content": "Say ok." }] });
    http.post_json("/v1/messages", &body, Duration::from_secs(60)).map(drop).map_err(verdict)
}

pub fn openai_http(base: &str, key: Option<&str>) -> Http {
    let headers = key.map(|k| vec![("authorization".to_string(), format!("Bearer {k}"))]).unwrap_or_default();
    Http::new(base, headers)
}

/// The models an OpenAI-compatible server offers, without Gemini's
/// `models/` prefix.
pub fn openai_models(http: &Http, timeout: Duration) -> Result<Vec<String>, Verdict> {
    let v = http.get_json_within("/models", timeout).map_err(verdict)?;
    Ok(ids(&v))
}

pub fn openai_try(http: &Http, model: &str, timeout: Duration) -> Result<(), Verdict> {
    let body = json!({ "model": model, "max_tokens": 16, "messages": [{ "role": "user", "content": "Say ok." }] });
    http.post_json("/chat/completions", &body, timeout).map(drop).map_err(verdict)
}

fn ids(v: &Value) -> Vec<String> {
    v["data"]
        .as_array()
        .map(|a| a.iter().filter_map(|m| m["id"].as_str()).map(|id| id.strip_prefix("models/").unwrap_or(id).to_string()).collect())
        .unwrap_or_default()
}

/// Gemini models that chat and call tools: not the speech, picture, video,
/// embedding or live-audio ones.
pub fn gemini_chat_models(models: &[String]) -> Vec<String> {
    const NOT_CHAT: [&str; 13] =
        ["tts", "image", "imagen", "veo", "embed", "aqa", "live", "audio", "robotics", "computer-use", "gemma", "learnlm", "lyria"];
    models.iter().filter(|m| m.starts_with("gemini") && !NOT_CHAT.iter().any(|w| m.contains(w))).cloned().collect()
}

/// The model to suggest instead of `wanted` (empty: none in mind): the
/// same family (flash, flash-lite, pro; opus, sonnet, haiku), stable before
/// preview, newest first. A `-latest` alias comes first, since it can't be
/// retired.
pub fn suggest(models: &[String], wanted: &str) -> Option<String> {
    if models.iter().any(|m| m == wanted) {
        return Some(wanted.to_string());
    }
    let families = ["flash-lite", "flash", "pro", "opus", "sonnet", "haiku"];
    let family = families.iter().find(|f| wanted.contains(*f)).copied();
    let in_family = |m: &&String| match family {
        Some("flash") => m.contains("flash") && !m.contains("lite"),
        Some(f) => m.contains(f),
        None => true,
    };
    let unstable = |m: &str| ["preview", "exp", "thinking"].iter().any(|w| m.contains(w));
    let mut pool: Vec<&String> = models.iter().filter(in_family).collect();
    if pool.is_empty() {
        pool = models.iter().collect();
    }
    // Claude lists newest first; Gemini names carry their version.
    let rank = |m: &str| (!m.ends_with("-latest"), unstable(m), std::cmp::Reverse(version(m)));
    let mut ranked: Vec<(usize, &String)> = pool.into_iter().enumerate().collect();
    ranked.sort_by(|(i, a), (j, b)| rank(a).cmp(&rank(b)).then(i.cmp(j)));
    ranked.first().map(|(_, m)| m.to_string())
}

/// `gemini-2.5-flash` → [2, 5]; nothing for names without one.
fn version(m: &str) -> Vec<u32> {
    let Some(rest) = m.strip_prefix("gemini-") else { return Vec::new() };
    rest.split('-').next().unwrap_or_default().split('.').map_while(|p| p.parse().ok()).collect()
}

/// A model server answering on this machine, and its models.
pub fn find_local(urls: &[String]) -> Option<(String, Vec<String>)> {
    urls.iter().find_map(|u| {
        let models = openai_models(&openai_http(u, None), Duration::from_secs(2)).ok()?;
        Some((u.clone(), models))
    })
}

// ---------------------------------------------------------------------------
// Writing orion.toml.

/// One change to orion.toml: a dotted key, set or removed.
#[derive(Debug, Clone)]
pub enum Edit {
    Set(&'static str, toml_edit::Value),
    Remove(&'static str),
}

pub fn set(key: &'static str, v: impl Into<toml_edit::Value>) -> Edit {
    Edit::Set(key, v.into())
}

/// `text` (or the example policy, when there is none) with `edits` made,
/// comments and layout kept, and checked to be a policy Orion takes.
pub fn edited(text: Option<&str>, edits: &[Edit]) -> Result<String> {
    let base = text.unwrap_or(include_str!("../orion.example.toml"));
    let mut doc: toml_edit::DocumentMut = base.parse().context("the policy isn't valid TOML")?;
    for e in edits {
        let key = match e {
            Edit::Set(k, _) | Edit::Remove(k) => *k,
        };
        let mut parts: Vec<&str> = key.split('.').collect();
        let last = parts.pop().expect("a key");
        let mut table = doc.as_table_mut();
        for p in parts {
            let item = table.entry(p).or_insert_with(|| {
                let mut t = toml_edit::Table::new();
                t.set_implicit(true);
                toml_edit::Item::Table(t)
            });
            table = item.as_table_mut().with_context(|| format!("{p} in the policy isn't a table"))?;
        }
        match e {
            Edit::Set(_, v) => {
                let mut v = v.clone();
                // Keep the comment after the old value.
                if let Some(old) = table.get(last).and_then(|i| i.as_value()) {
                    *v.decor_mut() = old.decor().clone();
                }
                table.insert(last, toml_edit::Item::Value(v));
            }
            Edit::Remove(_) => {
                table.remove(last);
            }
        }
    }
    let out = doc.to_string();
    Policy::from_toml(&out).context("the edited policy doesn't load")?;
    Ok(out)
}

/// Make `edits` to the policy at `path`, keeping the old one as `.bak`.
/// Returns the backup's path, if there was a file before.
pub fn edit_config(path: &Path, edits: &[Edit]) -> Result<Option<PathBuf>> {
    let old = match std::fs::read_to_string(path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let new = edited(old.as_deref(), edits)?;
    replace(path, old.as_deref(), &new)
}

/// Write `new` to `path`, keeping `old` (if any, and different) as `.bak`.
/// Both keep the file's permissions: a private profile stays private.
pub fn replace(path: &Path, old: Option<&str>, new: &str) -> Result<Option<PathBuf>> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mode = std::fs::metadata(path).map(|m| m.permissions().mode() & 0o7777).ok();
    let write = |p: &Path, text: &str| -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::remove_file(p);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode.unwrap_or(0o644))
            .open(p)
            .with_context(|| format!("writing {}", p.display()))?;
        if let Some(m) = mode {
            f.set_permissions(std::fs::Permissions::from_mode(m))?;
        }
        f.write_all(text.as_bytes()).with_context(|| format!("writing {}", p.display()))
    };
    let backup = match old {
        Some(old) if old != new => {
            let bak = PathBuf::from(format!("{}.bak", path.display()));
            write(&bak, old)?;
            Some(bak)
        }
        _ => None,
    };
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    write(&tmp, new)?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(backup)
}

/// `~/...` for paths under the home directory.
pub fn tilde(p: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if p.starts_with(&home) && home != Path::new("/") => {
            format!("~/{}", p.strip_prefix(&home).unwrap_or(p).display())
        }
        _ => p.display().to_string(),
    }
}

/// `…wxyz`: enough of a key to tell which one it is.
pub fn key_tail(key: &str) -> String {
    let tail: String = key.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("…{tail} ({} characters)", key.chars().count())
}

// ---------------------------------------------------------------------------
// The wizard.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provider {
    Claude,
    Gemini,
    Local,
}

/// What setup decided, before it is written.
struct Plan {
    edits: Vec<Edit>,
    /// The variable to save the key in, and the key.
    secret: Option<(&'static str, String)>,
    summary: Vec<String>,
}

pub fn run(explicit: Option<&Path>, opts: Options) -> Result<()> {
    let mut ask = Prompt::open(opts.yes).context("orion setup asks questions, so run it in a terminal")?;
    let path = explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("ORION_CONFIG").map(PathBuf::from))
        .or_else(Policy::default_config_path)
        .context("no home directory to keep the policy in")?;
    let env_path = envfile::path().context("no home directory to keep the env file in")?;
    println!("Orion setup\n");

    // The policy as it is, which setup changes rather than replaces.
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut current = Policy::from_toml("")?;
    let mut text = text;
    if let Some(t) = &text {
        match Policy::from_toml(t) {
            Ok(p) => current = p,
            Err(e) => {
                println!("{} doesn't load: {e:#}\n", tilde(&path));
                if t.parse::<toml_edit::DocumentMut>().is_err() {
                    if !ask.yes(&format!("Start it again from the example (the old one is kept as {}.bak)?", tilde(&path)), true)? {
                        bail!("left {} as it is; `orion doctor` can help find what's wrong", tilde(&path));
                    }
                    replace(&path, Some(t), include_str!("../orion.example.toml"))?;
                    text = Some(include_str!("../orion.example.toml").to_string());
                } else {
                    println!("Setup will change only the model settings, and checks the result loads.\n");
                }
            }
        }
    }

    let provider = match opts.provider.as_deref() {
        Some("claude") => Provider::Claude,
        Some("gemini") => Provider::Gemini,
        Some("local") => Provider::Local,
        Some(other) => bail!("--provider is claude, gemini or local, not {other:?}"),
        None => {
            let options = [
                "Claude (an Anthropic API key, from console.anthropic.com)".to_string(),
                "Gemini (a Google AI Studio key; the free tier works)".to_string(),
                "A model on this machine (Ollama, llama.cpp, vLLM, LM Studio)".to_string(),
            ];
            let default = match (current.chat.mode, current.chat.local.base_url.contains("googleapis")) {
                (crate::chat::Mode::Local, true) => 1,
                (crate::chat::Mode::Local, false) => 2,
                _ => 0,
            };
            let chosen = match ask.choose("Which model should Orion talk to?", &options, default)? {
                0 => Provider::Claude,
                1 => Provider::Gemini,
                _ => Provider::Local,
            };
            println!();
            chosen
        }
    };

    let plan = match provider {
        Provider::Claude => claude(&mut ask, &opts, &current)?,
        Provider::Gemini => gemini(&mut ask, &opts, &current)?,
        Provider::Local => local(&mut ask, &opts, &current)?,
    };

    // Write it all.
    let new = edited(text.as_deref(), &plan.edits)?;
    let backup = replace(&path, text.as_deref(), &new)?;
    println!("\nSaved {}: {}", tilde(&path), plan.summary.join(", "));
    if let Some(bak) = backup {
        println!("  (the previous one is {})", tilde(&bak));
    }
    if let Some((var, key)) = &plan.secret {
        envfile::set(&env_path, var, key)?;
        println!("Saved {var} in {}, readable only by you", tilde(&env_path));
        shadowed(&mut ask, var, key)?;
        // This process, and anything it starts, uses the new key.
        std::env::set_var(var, key);
    }
    restart_services(&mut ask)?;

    println!("\nDone. Try it: orion chat");
    println!("`orion doctor` checks the rest: the microphone, the sandbox and the helper.");
    Ok(())
}

/// A key to use: the one in `var` already (when it works), or a pasted one.
/// `check` tries a key; it returns what to tell the person when it fails.
fn get_key(
    ask: &mut Prompt,
    var: &'static str,
    paste: &str,
    check: &mut dyn FnMut(&str) -> Result<(), Verdict>,
) -> Result<String> {
    if let Ok(Some(have)) = crate::chat::http::secret_env(var) {
        let from = if envfile::loaded().iter().any(|n| n == var) { "in the env file" } else { "in your environment" };
        print!("Checking the {var} {from} ({})... ", key_tail(&have));
        std::io::stdout().flush()?;
        match check(&have) {
            Ok(()) => {
                println!("it works.");
                if ask.yes("Use it?", true)? {
                    return Ok(have);
                }
            }
            Err(v) => println!("it doesn't: {v}"),
        }
    } else if ask.assume_yes {
        bail!("--yes takes the key from {var}, which isn't set");
    }
    for _ in 0..3 {
        let raw = ask.secret(paste)?;
        let key = match clean_secret(&raw) {
            Ok(Some(k)) => k,
            Ok(None) => bail!("no key given; nothing was changed"),
            Err(e) => {
                println!("  That key {e}");
                continue;
            }
        };
        if key.len() != raw.trim().len() {
            println!("  (dropped spaces or invisible characters that came with the paste)");
        }
        print!("  Checking {} with a live call... ", key_tail(&key));
        std::io::stdout().flush()?;
        match check(&key) {
            Ok(()) => {
                println!("it works.");
                return Ok(key);
            }
            Err(Verdict::Rejected(m)) => println!("turned down: {m}"),
            Err(Verdict::Limited(m)) => {
                println!("it works, but its quota is used up for now ({m}).");
                return Ok(key);
            }
            Err(v) => {
                println!("couldn't check it: {v}");
                if ask.yes("  Save it anyway?", false)? {
                    return Ok(key);
                }
            }
        }
    }
    bail!("no working key after three tries; nothing was changed")
}

/// Pick a model from `models`: `--model`, else the configured one if it's
/// still offered, else a suggestion.
fn pick_model(ask: &mut Prompt, opts: &Options, models: &[String], configured: &str, fallback: &str) -> Result<String> {
    if let Some(m) = &opts.model {
        if !models.is_empty() && !models.contains(m) {
            println!("  {m} isn't among the models offered; using it as given.");
        }
        return Ok(m.clone());
    }
    if models.is_empty() {
        return Ok(if configured.is_empty() { fallback.to_string() } else { configured.to_string() });
    }
    if !configured.is_empty() && !models.iter().any(|m| m == configured) {
        println!("  The configured model, {configured}, isn't offered any more (retired, or misspelled).");
    }
    let wanted = if configured.is_empty() { fallback } else { configured };
    let best = suggest(models, wanted).unwrap_or_else(|| models[0].clone());
    // The suggestion, then the rest, a screenful at most.
    let mut shown = vec![best.clone()];
    shown.extend(models.iter().filter(|m| **m != best).take(9).cloned());
    let labels: Vec<String> =
        shown.iter().enumerate().map(|(i, m)| if i == 0 { format!("{m} (suggested)") } else { m.clone() }).collect();
    let i = ask.choose("Which model?", &labels, 0)?;
    Ok(shown[i].clone())
}

fn claude(ask: &mut Prompt, opts: &Options, current: &Policy) -> Result<Plan> {
    let base = crate::chat::http::non_empty_env("ANTHROPIC_BASE_URL").unwrap_or_else(|| current.chat.claude.base_url.clone());
    let mut models = Vec::new();
    let key = get_key(ask, "ANTHROPIC_API_KEY", "Paste your Anthropic API key (it won't show): ", &mut |k| {
        models = claude_models(&claude_http(&base, &ClaudeAuth::Key(k.to_string())))?;
        Ok(())
    })?;
    let http = claude_http(&base, &ClaudeAuth::Key(key.clone()));
    if models.is_empty() {
        models = claude_models(&http).unwrap_or_default();
    }
    let model = pick_model(ask, opts, &models, &current.chat.claude.model, "claude-opus-5-5")?;
    print!("Asking {model} to say ok... ");
    std::io::stdout().flush()?;
    match claude_try(&http, &model) {
        Ok(()) => println!("it answered."),
        Err(Verdict::Limited(m)) => println!("rate limited for now ({m}), but it's there."),
        Err(v) => bail!("{model} didn't answer: {v}"),
    }
    let mut edits = vec![set("chat.claude.model", model.as_str())];
    let mut summary = vec![format!("Claude ({model})")];
    // A model on this machine too, for private and simple messages.
    let mut provider = "claude";
    let urls = local_urls(current);
    print!("Looking for a model server on this machine... ");
    std::io::stdout().flush()?;
    match find_local(&urls) {
        Some((url, local_models)) if !local_models.is_empty() => {
            println!("{url} has {}.", local_models.join(", "));
            if ask.yes("Use it too, so private and simple messages stay on this machine?", true)? {
                provider = "auto";
                let m = pick_model(ask, &Options::default(), &local_models, &current.chat.local.model, &local_models[0])?;
                edits.extend(local_edits(current, &url, &m));
                summary.push(format!("{m} on this machine, chosen per message"));
            }
        }
        _ => println!("none."),
    }
    edits.insert(0, set("chat.provider", provider));
    Ok(Plan { edits, secret: Some(("ANTHROPIC_API_KEY", key)), summary })
}

fn gemini(ask: &mut Prompt, opts: &Options, current: &Policy) -> Result<Plan> {
    let mut models = Vec::new();
    let key = get_key(ask, "GEMINI_API_KEY", "Paste your Gemini API key from aistudio.google.com/apikey (it won't show): ", &mut |k| {
        models = gemini_chat_models(&openai_models(&openai_http(GEMINI_URL, Some(k)), Duration::from_secs(20))?);
        Ok(())
    })?;
    let http = openai_http(GEMINI_URL, Some(&key));
    if models.is_empty() {
        models = gemini_chat_models(&openai_models(&http, Duration::from_secs(20)).unwrap_or_default());
    }
    let configured = if current.chat.local.base_url.contains("googleapis") { current.chat.local.model.as_str() } else { "" };
    let model = pick_model(ask, opts, &models, configured, "gemini-flash-latest")?;
    print!("Asking {model} to say ok... ");
    std::io::stdout().flush()?;
    match openai_try(&http, &model, Duration::from_secs(60)) {
        Ok(()) => println!("it answered."),
        Err(Verdict::Limited(m)) => println!("rate limited for now ({m}), but it's there."),
        Err(v) => bail!("{model} didn't answer: {v}"),
    }
    let mut edits = vec![
        set("chat.provider", "local"),
        set("chat.local.base_url", GEMINI_URL),
        set("chat.local.model", model.as_str()),
        set("chat.local.api_key_env", "GEMINI_API_KEY"),
        Edit::Remove("chat.local.on_machine"),
    ];
    if current.chat.local.reasoning_effort.is_none() {
        edits.push(set("chat.local.reasoning_effort", "low"));
    }
    Ok(Plan { edits, secret: Some(("GEMINI_API_KEY", key)), summary: vec![format!("Gemini ({model})")] })
}

fn local(ask: &mut Prompt, opts: &Options, current: &Policy) -> Result<Plan> {
    let urls = match &opts.base_url {
        Some(u) => vec![u.clone()],
        None => local_urls(current),
    };
    print!("Looking for a model server on this machine... ");
    std::io::stdout().flush()?;
    let (url, models) = match find_local(&urls) {
        Some(found) => {
            println!("{}.", found.0);
            found
        }
        None => {
            println!("none answers.");
            println!("  Ollama is the simplest: curl -fsSL https://ollama.com/install.sh | sh && ollama pull qwen3:8b");
            let typed = if ask.assume_yes { String::new() } else { ask.line("Or type its address (like http://127.0.0.1:8080/v1), or Enter to stop: ")? };
            if typed.is_empty() {
                bail!("no model server; start one and run `orion setup` again");
            }
            let models = openai_models(&openai_http(&typed, None), Duration::from_secs(5)).map_err(|v| anyhow!("{typed}: {v}"))?;
            (typed, models)
        }
    };
    if models.is_empty() {
        bail!("{url} has no models yet; for Ollama: ollama pull qwen3:8b");
    }
    let configured = if current.chat.local.base_url == url { current.chat.local.model.as_str() } else { "" };
    let model = pick_model(ask, opts, &models, configured, &models[0])?;
    print!("Asking {model} to say ok (loading it can take a minute)... ");
    std::io::stdout().flush()?;
    match openai_try(&openai_http(&url, None), &model, Duration::from_secs(300)) {
        Ok(()) => println!("it answered."),
        Err(v) => bail!("{model} didn't answer: {v}"),
    }
    let mut edits = vec![set("chat.provider", "local")];
    edits.extend(local_edits(current, &url, &model));
    Ok(Plan { edits, secret: None, summary: vec![format!("{model} on this machine")] })
}

/// `[chat.local]` for a server on this machine, undoing a Gemini setup.
fn local_edits(current: &Policy, url: &str, model: &str) -> Vec<Edit> {
    let mut edits = vec![set("chat.local.base_url", url), set("chat.local.model", model)];
    if current.chat.local.base_url.contains("googleapis") {
        edits.extend([Edit::Remove("chat.local.api_key_env"), Edit::Remove("chat.local.reasoning_effort")]);
    }
    edits
}

fn local_urls(current: &Policy) -> Vec<String> {
    let mut urls = Vec::new();
    if !current.chat.local.base_url.contains("googleapis") {
        urls.push(current.chat.local.base_url.clone());
    }
    for u in LOCAL_URLS {
        if !urls.iter().any(|have| have == u) {
            urls.push(u.to_string());
        }
    }
    urls
}

/// A different `var` exported by the shell would win over the env file's;
/// offer to comment out the line that sets it.
fn shadowed(ask: &mut Prompt, var: &str, key: &str) -> Result<()> {
    let from_shell = std::env::var(var).ok().filter(|v| !v.is_empty() && !envfile::loaded().iter().any(|n| n == var));
    let Some(shell) = from_shell else { return Ok(()) };
    if clean_secret(&shell).ok().flatten().as_deref() == Some(key) {
        return Ok(());
    }
    let found = crate::doctor::profile_lines(var);
    println!("\nYour shell exports a different {var}, and that one wins over the env file.");
    for (file, line, _) in &found {
        println!("  {}:{line}", tilde(file));
    }
    if !found.is_empty() && ask.yes("Comment those lines out (each file is kept as .bak)?", true)? {
        for msg in crate::doctor::comment_out(&found)? {
            println!("  {msg}");
        }
    }
    println!("Then open a new terminal, or run: unset {var}");
    Ok(())
}

/// The services read the env file when they start: restart the running ones.
fn restart_services(ask: &mut Prompt) -> Result<()> {
    let units = ["orion.service", "orion-shell.service", "orion-voice.service"];
    let active: Vec<&str> = units
        .into_iter()
        .filter(|u| {
            std::process::Command::new("systemctl")
                .args(["--user", "--quiet", "is-active", u])
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
        .collect();
    if active.is_empty() {
        return Ok(());
    }
    if ask.yes(&format!("Restart {} so it reads the new settings?", active.join(", ")), true)? {
        let ok = std::process::Command::new("systemctl").args(["--user", "restart"]).args(&active).status()?.success();
        println!("{}", if ok { "  restarted" } else { "  the restart failed; see systemctl --user status" });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn suggests_a_live_model_of_the_same_kind() {
        let gemini = s(&["gemini-2.5-flash", "gemini-3.8-flash", "gemini-3.8-flash-lite", "gemini-3.9-flash-preview", "gemini-3.8-pro"]);
        assert_eq!(suggest(&gemini, "gemini-3.8-flash").as_deref(), Some("gemini-3.8-flash"));
        // Retired: the newest stable flash, not a lite, pro or preview.
        assert_eq!(suggest(&gemini, "gemini-2.0-flash").as_deref(), Some("gemini-3.8-flash"));
        assert_eq!(suggest(&gemini, "gemini-2.0-flash-lite").as_deref(), Some("gemini-3.8-flash-lite"));
        let mut aliased = gemini.clone();
        aliased.push("gemini-flash-latest".into());
        assert_eq!(suggest(&aliased, "gemini-2.0-flash").as_deref(), Some("gemini-flash-latest"));
        // Claude lists newest first.
        let claude = s(&["claude-opus-5-5", "claude-sonnet-5-5", "claude-opus-5", "claude-haiku-5-5"]);
        assert_eq!(suggest(&claude, "claude-opus-4-1").as_deref(), Some("claude-opus-5-5"));
        assert_eq!(suggest(&claude, "claude-sonet").as_deref(), Some("claude-opus-5-5"));
        assert_eq!(suggest(&[], "x"), None);
    }

    #[test]
    fn keeps_only_gemini_models_that_chat() {
        let all = s(&["gemini-3.8-flash", "gemini-3.8-flash-preview-tts", "gemini-3.8-flash-image", "text-embedding-004",
                      "gemini-embedding-001", "gemma-3-27b-it", "imagen-4.0", "gemini-3.8-flash-live"]);
        assert_eq!(gemini_chat_models(&all), s(&["gemini-3.8-flash"]));
        assert_eq!(ids(&json!({"data": [{"id": "models/gemini-3.8-flash"}, {"id": "qwen3:8b"}]})), s(&["gemini-3.8-flash", "qwen3:8b"]));
    }

    #[test]
    fn edits_keep_comments_and_must_load() {
        let text = "# mine\n[chat]\nprovider = \"auto\"   # how\n\n[chat.local]\nmodel = \"old\"\napi_key_env = \"GEMINI_API_KEY\"\n";
        let out = edited(Some(text), &[set("chat.provider", "local"), set("chat.local.model", "new"), Edit::Remove("chat.local.api_key_env")])
            .unwrap();
        assert_eq!(out, "# mine\n[chat]\nprovider = \"local\"   # how\n\n[chat.local]\nmodel = \"new\"\n");
        // New tables, in a file that had none.
        let out = edited(Some(""), &[set("chat.claude.model", "claude-opus-5-5")]).unwrap();
        assert!(Policy::from_toml(&out).unwrap().chat.claude.model == "claude-opus-5-5", "{out}");
        // From the example, which keeps its comments.
        let out = edited(None, &[set("chat.provider", "claude")]).unwrap();
        assert!(out.contains("# Orion policy.") && out.contains("provider = \"claude\""), "{out}");
        // Nothing that doesn't load is written.
        assert!(edited(Some(""), &[set("chat.provider", 3)]).is_err());
    }

    #[test]
    fn backs_up_what_it_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("orion.toml");
        assert_eq!(edit_config(&p, &[set("chat.provider", "claude")]).unwrap(), None);
        std::fs::write(&p, "[chat]\nprovider = \"local\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        let bak = edit_config(&p, &[set("chat.provider", "claude")]).unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), "[chat]\nprovider = \"local\"\n");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "[chat]\nprovider = \"claude\"\n");
        // A private file's backup, and the file itself, stay private.
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&bak), mode(&p)), (0o600, 0o600));
    }

    #[test]
    fn prompts_take_defaults_and_numbers() {
        let mut p = Prompt::scripted("\nx\n2\n\nn\n");
        let opts = s(&["a", "b"]);
        assert_eq!(p.choose("?", &opts, 0).unwrap(), 0);
        assert_eq!(p.choose("?", &opts, 0).unwrap(), 1, "a bad answer is asked again");
        assert!(p.yes("?", true).unwrap());
        assert!(!p.yes("?", true).unwrap());
        assert!(p.yes("?", true).is_err(), "end of input is no answer");
    }

    #[test]
    fn tells_a_turned_down_key_from_a_missing_model() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let replies = [
                ("401 Unauthorized", r#"{"error":{"message":"invalid x-api-key"}}"#),
                ("404 Not Found", r#"{"error":{"message":"model: claude-old"}}"#),
                ("200 OK", r#"{"data":[{"id":"claude-opus-5-5"}]}"#),
            ];
            for (conn, (status, body)) in listener.incoming().zip(replies) {
                let mut conn = conn.unwrap();
                let mut buf = [0u8; 4096];
                let _ = std::io::Read::read(&mut conn, &mut buf);
                let reply = format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                std::io::Write::write_all(&mut conn, reply.as_bytes()).unwrap();
            }
        });
        let http = claude_http(&url, &ClaudeAuth::Key("k".into()));
        assert!(matches!(claude_models(&http), Err(Verdict::Rejected(_))));
        assert!(matches!(claude_try(&http, "claude-old"), Err(Verdict::NoModel(_))));
        assert_eq!(claude_models(&http).unwrap(), s(&["claude-opus-5-5"]));
    }
}
