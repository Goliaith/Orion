//! Automatic routing between Claude and the local model, one user message
//! at a time. Privacy decides first, then complexity, then availability.
//!
//! Privacy and follow-ups are always decided by rules. Complexity is judged
//! by a [`Classifier`] (the local model, under `strategy = "model"`) or by
//! keyword rules (`strategy = "rules"`, and whenever the classifier fails).
//! Every decision carries its reason, and `/route <text>` in the chat shows
//! it without sending the message to either model.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use super::Provider;
use crate::policy::{expand_home, resolve_lenient};

/// `[chat.router]` in orion.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouterConfig {
    /// How complexity is judged: `model` asks the local model, `rules`
    /// matches `complex_words` and the other heuristics.
    pub strategy: Strategy,
    /// The local model that classifies; empty means the chat's local model.
    /// A small fast one (such as qwen3:1.7b) keeps routing quick.
    pub classifier_model: String,
    pub classifier_timeout_secs: u64,
    /// Where a request goes when nothing points either way (and what the
    /// classifier is told to pick when unsure).
    pub prefer: Provider,
    /// Words that make a message private, so it stays on this machine.
    /// A word matches the start of a word in the message, ignoring case;
    /// patterns with spaces match anywhere.
    pub private_words: Vec<String>,
    /// Also treat messages containing something shaped like a key or token
    /// as private.
    pub detect_secrets: bool,
    /// Paths Claude must never see: its tool calls that mention them are
    /// refused, and messages that mention them stay local.
    pub private_paths: Vec<PathBuf>,
    /// Words that mark a request as complex enough for Claude.
    pub complex_words: Vec<String>,
    /// Requests longer than this many words go to Claude.
    pub long_request_words: usize,
}

impl Default for RouterConfig {
    fn default() -> Self {
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect();
        Self {
            strategy: Strategy::Model,
            classifier_model: String::new(),
            classifier_timeout_secs: 15,
            prefer: Provider::Local,
            private_words: words(&[
                "password", "passwd", "passphrase", "credential", "private key", "api key", "ssn",
                "social security", "credit card", "bank", "salary", "tax return", "medical", "diagnos",
                "therapy", "prescription",
            ]),
            detect_secrets: true,
            private_paths: ["~/.ssh", "~/.gnupg", "~/.password-store", "~/.aws", "~/.netrc"]
                .iter()
                .map(PathBuf::from)
                .collect(),
            complex_words: words(&[
                "code", "script", "program", "debug", "refactor", "implement", "design", "architect",
                "plan", "analyze", "analyse", "explain", "compare", "research", "optimize", "optimise",
                "write", "draft", "review", "why", "strategy", "algorithm", "step by step",
            ]),
            long_request_words: 60,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    Model,
    Rules,
}

/// A classifier's call on one message.
#[derive(Debug, PartialEq, Eq)]
pub struct Verdict {
    pub provider: Provider,
    pub reason: String,
}

/// Judges whether a message needs Claude. Only ever shown messages that
/// passed the privacy rules, and it must run on this machine.
pub trait Classifier: Send {
    /// The model doing the judging, for display.
    fn name(&self) -> String;
    /// `previous` is the last message and which model answered it.
    fn classify(&self, text: &str, previous: Option<(Provider, &str)>) -> Result<Verdict>;
}

/// After this many failures in a row, the classifier is left out for the
/// rest of the session, so a dead server doesn't cost a timeout per message.
const CLASSIFIER_FAILURE_LIMIT: u32 = 3;

/// Where a message should go.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    To { provider: Provider, reason: String },
    /// The message looks private but no local model is available. Only the
    /// user can decide to send it to Claude anyway.
    PrivateButNoLocal { reason: String },
}

pub struct RouteContext<'a> {
    /// Which model answered the previous message.
    pub previous: Option<Provider>,
    /// The previous message itself, as context for the classifier.
    pub previous_text: Option<&'a str>,
    pub claude_available: bool,
    pub local_available: bool,
}

/// Short replies that continue the previous exchange rather than start a
/// new one.
const FOLLOW_UPS: [&str; 16] = [
    "yes", "yeah", "yep", "ok", "okay", "sure", "no", "nope", "do it", "go ahead", "continue", "thanks",
    "thank you", "and", "also", "then",
];
const FOLLOW_UP_WORDS: usize = 6;
const SHORT_REQUEST_WORDS: usize = 12;

pub struct Router {
    config: RouterConfig,
    /// `private_paths` as written (with `~`), expanded, and canonicalized.
    private_forms: Vec<(String, PathBuf)>,
    classifier_failures: Cell<u32>,
}

impl Router {
    pub fn new(config: RouterConfig) -> Self {
        let mut private_forms = Vec::new();
        for p in &config.private_paths {
            let expanded = expand_home(p);
            let real = expanded.canonicalize().unwrap_or_else(|_| expanded.clone());
            for path in [expanded, real] {
                private_forms.push((p.to_string_lossy().into_owned(), path));
            }
        }
        Self { config, private_forms, classifier_failures: Cell::new(0) }
    }

    /// Whether the classifier is consulted at all right now.
    pub fn uses_classifier(&self) -> bool {
        self.config.strategy == Strategy::Model && self.classifier_failures.get() < CLASSIFIER_FAILURE_LIMIT
    }

    pub fn config(&self) -> &RouterConfig {
        &self.config
    }

    /// Why `text` is private, if it is.
    pub fn private_reason(&self, text: &str) -> Option<String> {
        let lower = text.to_lowercase();
        if let Some(w) = self.config.private_words.iter().find(|w| mentions(&lower, w)) {
            return Some(format!("mentions \"{w}\""));
        }
        if self.config.detect_secrets && contains_secret(text) {
            return Some("contains something that looks like a key or token".into());
        }
        if let Some((written, _)) = self.private_forms.iter().find(|(written, path)| {
            text.contains(written.as_str()) || text.contains(path.to_string_lossy().as_ref())
        }) {
            return Some(format!("mentions the private path {written}"));
        }
        None
    }

    pub fn route(&self, text: &str, ctx: &RouteContext, classifier: Option<&dyn Classifier>) -> Route {
        if let Some(why) = self.private_reason(text) {
            let reason = format!("private: {why}");
            return if ctx.local_available {
                Route::To { provider: Provider::Local, reason }
            } else {
                Route::PrivateButNoLocal { reason }
            };
        }
        let (wanted, reason) = match self.follow_up(text, ctx.previous) {
            Some(prev) => (prev, "follow-up".to_string()),
            None => self.judge(text, ctx, classifier),
        };
        let available = |p| match p {
            Provider::Claude => ctx.claude_available,
            Provider::Local => ctx.local_available,
        };
        if available(wanted) {
            return Route::To { provider: wanted, reason };
        }
        let other = other(wanted);
        Route::To { provider: other, reason: format!("{reason}, but {} is unavailable", name(wanted)) }
    }

    /// The previous model, when `text` just continues that exchange.
    fn follow_up(&self, text: &str, previous: Option<Provider>) -> Option<Provider> {
        let prev = previous?;
        let lower = text.to_lowercase();
        let trimmed = lower.trim_start_matches(|c: char| !c.is_alphanumeric());
        let short = text.split_whitespace().count() <= FOLLOW_UP_WORDS;
        (short && !text.contains("```") && FOLLOW_UPS.iter().any(|f| starts_with_word(trimmed, f))).then_some(prev)
    }

    /// Complexity: the classifier's call when there is a choice to make,
    /// else the keyword rules.
    fn judge(&self, text: &str, ctx: &RouteContext, classifier: Option<&dyn Classifier>) -> (Provider, String) {
        let both = ctx.claude_available && ctx.local_available;
        let (Some(c), true, true) = (classifier, both, self.uses_classifier()) else {
            return self.rules(text);
        };
        let start = Instant::now();
        let previous = ctx.previous.zip(ctx.previous_text);
        match c.classify(text, previous) {
            Ok(v) => {
                self.classifier_failures.set(0);
                let secs = start.elapsed().as_secs_f64();
                (v.provider, format!("{}: {} ({secs:.1}s)", c.name(), v.reason))
            }
            // Ctrl-C while classifying is not the classifier's fault.
            Err(e) if crate::interrupt::is_interrupt(&e) => self.rules(text),
            Err(e) => {
                let failures = self.classifier_failures.get() + 1;
                self.classifier_failures.set(failures);
                let (p, why) = self.rules(text);
                let first = format!("{e:#}");
                let first = first.lines().next().unwrap_or_default().chars().take(80).collect::<String>();
                let note = if failures >= CLASSIFIER_FAILURE_LIMIT {
                    format!("classifier failed {failures} times, using rules from now on: {first}")
                } else {
                    format!("classifier failed: {first}")
                };
                (p, format!("{why}; {note}"))
            }
        }
    }

    /// The keyword rules for complexity.
    fn rules(&self, text: &str) -> (Provider, String) {
        let lower = text.to_lowercase();
        let words = text.split_whitespace().count();
        if text.contains("```") || looks_like_code(text) {
            return (Provider::Claude, "contains code".into());
        }
        if let Some(w) = self.config.complex_words.iter().find(|w| mentions(&lower, w)) {
            return (Provider::Claude, format!("complex: \"{w}\""));
        }
        if words > self.config.long_request_words {
            return (Provider::Claude, format!("long request ({words} words)"));
        }
        if text.matches('?').count() >= 2 || has_numbered_steps(text) {
            return (Provider::Claude, "several questions or steps".into());
        }
        if words <= SHORT_REQUEST_WORDS {
            return (self.config.prefer, "short request".into());
        }
        (self.config.prefer, "nothing complex".into())
    }

    /// A private path that the string arguments of a tool call refer to.
    /// Relative arguments are resolved against `cwd` (or Orion's working
    /// directory), so `cat .ssh/id_rsa` from `~` is caught too.
    pub fn private_path_in(&self, input: &Value) -> Option<String> {
        let cwd = input
            .get("cwd")
            .and_then(Value::as_str)
            .map(|c| expand_home(Path::new(c)))
            .or_else(|| std::env::current_dir().ok());
        let mut strings = Vec::new();
        match input {
            // What fs_write writes reveals nothing; where it writes is in `path`.
            Value::Object(o) => o.iter().filter(|(k, _)| *k != "content").for_each(|(_, v)| collect_strings(v, &mut strings)),
            other => collect_strings(other, &mut strings),
        }
        for s in strings {
            for (written, root) in &self.private_forms {
                if s.contains(written.as_str()) || s.contains(root.to_string_lossy().as_ref()) {
                    return Some(written.clone());
                }
                // Also each `--flag=value` / bare value as a path, resolved
                // and canonicalized so symlinks into a private tree count.
                let value = s.rsplit('=').next().unwrap_or(s);
                let path = expand_home(Path::new(value));
                let path = if path.is_absolute() {
                    path
                } else if let Some(cwd) = &cwd {
                    cwd.join(path)
                } else {
                    continue;
                };
                let real = resolve_lenient(&path).unwrap_or(path);
                if real.starts_with(root) {
                    return Some(written.clone());
                }
            }
        }
        None
    }
}

pub fn other(p: Provider) -> Provider {
    match p {
        Provider::Claude => Provider::Local,
        Provider::Local => Provider::Claude,
    }
}

pub fn name(p: Provider) -> &'static str {
    match p {
        Provider::Claude => "Claude",
        Provider::Local => "the local model",
    }
}

fn collect_strings<'v>(v: &'v Value, out: &mut Vec<&'v str>) {
    match v {
        Value::String(s) => out.push(s),
        Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        Value::Object(o) => o.values().for_each(|x| collect_strings(x, out)),
        _ => {}
    }
}

/// `pattern` (lowercase) occurs in `lower` at the start of a word; patterns
/// with spaces may occur anywhere.
fn mentions(lower: &str, pattern: &str) -> bool {
    let pattern = pattern.to_lowercase();
    if pattern.contains(' ') {
        return lower.contains(&pattern);
    }
    lower.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(&pattern))
}

fn starts_with_word(text: &str, word: &str) -> bool {
    text.strip_prefix(word).is_some_and(|rest| rest.is_empty() || !rest.starts_with(char::is_alphanumeric))
}

fn looks_like_code(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    let codey = lines
        .iter()
        .filter(|l| {
            let t = l.trim();
            t.ends_with(';') || t.ends_with('{') || t == "}" || t.starts_with("fn ") || t.starts_with("def ")
        })
        .count();
    lines.len() >= 2 && codey >= 2
}

fn has_numbered_steps(text: &str) -> bool {
    let steps = text
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            let digits = t.chars().take_while(char::is_ascii_digit).count();
            digits > 0 && matches!(t[digits..].chars().next(), Some('.') | Some(')'))
        })
        .count();
    steps >= 2
}

/// Something shaped like a credential: a known key prefix, a PEM private
/// key, or a long random-looking token mixing upper, lower and digits.
pub fn contains_secret(text: &str) -> bool {
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY") {
        return true;
    }
    const PREFIXES: [&str; 9] = ["sk-", "ghp_", "gho_", "ghs_", "github_pat_", "glpat-", "xoxb-", "xoxp-", "AIza"];
    text.split(|c: char| c.is_whitespace() || "\"'`,;()[]{}<>".contains(c)).any(|tok| {
        let tok = tok.rsplit('=').next().unwrap_or(tok);
        if PREFIXES.iter().any(|p| tok.starts_with(p) && tok.len() >= p.len() + 16) {
            return true;
        }
        if tok.len() == 20 && tok.starts_with("AKIA") && tok.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
            return true;
        }
        tok.len() >= 32
            && tok.chars().all(|c| c.is_ascii_alphanumeric() || "+/=_-".contains(c))
            && tok.chars().any(|c| c.is_ascii_digit())
            && tok.chars().any(|c| c.is_ascii_uppercase())
            && tok.chars().any(|c| c.is_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn both() -> RouteContext<'static> {
        RouteContext { previous: None, previous_text: None, claude_available: true, local_available: true }
    }

    fn to(r: Route) -> (Provider, String) {
        match r {
            Route::To { provider, reason } => (provider, reason),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn simple_stays_local_and_complex_goes_to_claude() {
        let r = Router::new(RouterConfig::default());
        assert_eq!(to(r.route("how full is my disk?", &both(), None)).0, Provider::Local);
        assert_eq!(to(r.route("list the biggest processes", &both(), None)).0, Provider::Local);
        let (p, why) = to(r.route("debug why my nginx config fails", &both(), None));
        assert_eq!(p, Provider::Claude);
        assert!(why.contains("debug"), "{why}");
        assert_eq!(to(r.route("fn main() {\n    let x = 1;\n}", &both(), None)).0, Provider::Claude);
        assert_eq!(to(r.route("1. back up home\n2. clean caches\n3. report", &both(), None)).0, Provider::Claude);
        let long = "word ".repeat(70);
        assert!(to(r.route(&long, &both(), None)).1.starts_with("long request"));
    }

    #[test]
    fn privacy_wins_over_complexity() {
        let r = Router::new(RouterConfig::default());
        let (p, why) = to(r.route("write a script that rotates my bank passwords", &both(), None));
        assert_eq!(p, Provider::Local);
        assert!(why.starts_with("private"), "{why}");
        assert_eq!(to(r.route("my key is sk-ant-api03-abcdefghijklmnopqrstuv", &both(), None)).0, Provider::Local);
        assert_eq!(to(r.route("what's in ~/.ssh/config", &both(), None)).0, Provider::Local);

        let no_local = RouteContext { local_available: false, ..both() };
        assert!(matches!(r.route("my password is hunter2", &no_local, None), Route::PrivateButNoLocal { .. }));
    }

    #[test]
    fn follow_ups_stay_with_the_previous_model() {
        let r = Router::new(RouterConfig::default());
        let after_claude = RouteContext { previous: Some(Provider::Claude), ..both() };
        assert_eq!(to(r.route("yes, do it", &after_claude, None)), (Provider::Claude, "follow-up".into()));
        assert_eq!(to(r.route("ok", &after_claude, None)).0, Provider::Claude);
        // Not a follow-up word, so it's routed on its own merits.
        assert_eq!(to(r.route("show uptime", &after_claude, None)).0, Provider::Local);
        assert_eq!(to(r.route("nothing", &after_claude, None)).0, Provider::Local, "\"no\" must be a whole word");
    }

    #[test]
    fn falls_back_when_one_side_is_down() {
        let r = Router::new(RouterConfig::default());
        let no_claude = RouteContext { claude_available: false, ..both() };
        let (p, why) = to(r.route("refactor this module", &no_claude, None));
        assert_eq!(p, Provider::Local);
        assert!(why.contains("unavailable"));
        let no_local = RouteContext { local_available: false, ..both() };
        assert_eq!(to(r.route("uptime?", &no_local, None)).0, Provider::Claude);
    }

    struct Fake {
        answer: Result<Provider, String>,
        calls: Cell<u32>,
        seen_previous: Cell<bool>,
    }

    impl Fake {
        fn new(answer: Result<Provider, &str>) -> Self {
            Self { answer: answer.map_err(str::to_string), calls: Cell::new(0), seen_previous: Cell::new(false) }
        }
    }

    impl Classifier for Fake {
        fn name(&self) -> String {
            "tiny".into()
        }
        fn classify(&self, _text: &str, previous: Option<(Provider, &str)>) -> Result<Verdict> {
            self.calls.set(self.calls.get() + 1);
            self.seen_previous.set(previous.is_some());
            match &self.answer {
                Ok(p) => Ok(Verdict { provider: *p, reason: "judged".into() }),
                Err(e) => Err(anyhow::anyhow!("{e}")),
            }
        }
    }

    #[test]
    fn classifier_overrules_keywords_but_not_privacy() {
        let r = Router::new(RouterConfig::default());
        // "write" is a complex keyword, but the classifier says it's simple.
        let local = Fake::new(Ok(Provider::Local));
        let (p, why) = to(r.route("write hello to a note", &both(), Some(&local)));
        assert_eq!(p, Provider::Local);
        assert!(why.starts_with("tiny: judged ("), "{why}");

        // Privacy is decided before the classifier is asked.
        let claude = Fake::new(Ok(Provider::Claude));
        assert_eq!(to(r.route("my password is hunter2", &both(), Some(&claude))).0, Provider::Local);
        // So are follow-ups.
        let ctx = RouteContext { previous: Some(Provider::Local), previous_text: Some("disk?"), ..both() };
        assert_eq!(to(r.route("yes please", &ctx, Some(&claude))).0, Provider::Local);
        assert_eq!(claude.calls.get(), 0);

        // It gets the previous message as context.
        assert_eq!(to(r.route("now make it faster", &ctx, Some(&claude))).0, Provider::Claude);
        assert!(claude.seen_previous.get());
    }

    #[test]
    fn classifier_is_skipped_when_there_is_no_choice() {
        let r = Router::new(RouterConfig::default());
        let c = Fake::new(Ok(Provider::Claude));
        let no_claude = RouteContext { claude_available: false, ..both() };
        assert_eq!(to(r.route("refactor it", &no_claude, Some(&c))).0, Provider::Local);
        let rules = Router::new(RouterConfig { strategy: Strategy::Rules, ..Default::default() });
        to(rules.route("refactor it", &both(), Some(&c)));
        assert_eq!(c.calls.get(), 0);
    }

    #[test]
    fn failing_classifier_falls_back_then_switches_off() {
        let r = Router::new(RouterConfig::default());
        let broken = Fake::new(Err("connection refused"));
        let (p, why) = to(r.route("debug my build", &both(), Some(&broken)));
        assert_eq!(p, Provider::Claude, "rules still route it");
        assert!(why.contains("classifier failed: connection refused"), "{why}");
        to(r.route("debug my build", &both(), Some(&broken)));
        let (_, why) = to(r.route("debug my build", &both(), Some(&broken)));
        assert!(why.contains("using rules from now on"), "{why}");
        assert!(!r.uses_classifier());
        to(r.route("debug my build", &both(), Some(&broken)));
        assert_eq!(broken.calls.get(), 3);
    }

    #[test]
    fn spots_secrets() {
        assert!(contains_secret("token=ghp_0123456789abcdefABCDEF0123456789abcd"));
        assert!(contains_secret("AKIAIOSFODNN7EXAMPLE"));
        assert!(contains_secret("-----BEGIN OPENSSH PRIVATE KEY-----\nabc"));
        assert!(contains_secret("use Zx9kQ2mW7pL4vB8nR3tY6uI1oP5aS0dF here"));
        assert!(!contains_secret("commit 3f2a9c1e5b7d4a6f8e0c2b4d6f8a0c2e4b6d8f0a"), "lowercase hex is a hash, not a key");
        assert!(!contains_secret("how much RAM do I have"));
    }

    #[test]
    fn guards_private_paths_in_tool_calls() {
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        std::os::unix::fs::symlink(&ssh, home.path().join("keys")).unwrap();
        let config = RouterConfig { private_paths: vec![ssh.clone()], ..Default::default() };
        let r = Router::new(config);
        let h = home.path().to_str().unwrap();

        assert!(r.private_path_in(&json!({ "path": format!("{h}/.ssh/id_rsa") })).is_some());
        assert!(r.private_path_in(&json!({ "path": format!("{h}/keys/id_rsa") })).is_some(), "via symlink");
        assert!(r.private_path_in(&json!({ "command": "cat", "args": [".ssh/id_rsa"], "cwd": h })).is_some());
        assert!(r.private_path_in(&json!({ "command": "x", "args": [format!("--key={h}/.ssh/k")] })).is_some());
        assert!(r.private_path_in(&json!({ "path": format!("{h}/notes.txt") })).is_none());
        assert!(r.private_path_in(&json!({ "command": "df", "args": ["-h"] })).is_none());
        assert!(r.private_path_in(&json!({ "path": format!("{h}/n.txt"), "content": format!("see {h}/.ssh") })).is_none());
    }
}
