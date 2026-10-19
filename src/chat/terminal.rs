//! The interactive terminal: line editing, streamed output, tool activity
//! and approval prompts.

use std::collections::HashSet;
use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use anyhow::Result;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use serde_json::Value;

use super::{router, Mode, Provider, Session, Ui};
use crate::{Approver, Decision, Orion, Tool};

struct Style {
    on: bool,
}

impl Style {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.on { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
    fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
    fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
    fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
    fn cyan(&self, s: &str) -> String {
        self.paint("36", s)
    }
}

/// What was printed last, so streams of different kinds start on fresh lines.
#[derive(PartialEq, Clone, Copy)]
enum Last {
    Line,
    Text,
    Thinking,
}

pub struct Terminal {
    style: Style,
    last: Last,
    /// Tools the user approved for the rest of the session.
    always: HashSet<String>,
    /// When the last prompt was answered, so tool timings leave out the
    /// time the user took to decide.
    answered_at: Option<Instant>,
}

impl Terminal {
    pub fn new() -> Self {
        let on = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        Self { style: Style { on }, last: Last::Line, always: HashSet::new(), answered_at: None }
    }

    fn line_break(&mut self) {
        if self.last != Last::Line {
            println!();
            self.last = Last::Line;
        }
    }

    fn ask(&mut self, tool: &dyn Tool, args: &Value) -> bool {
        if self.always.contains(tool.name()) {
            return true;
        }
        self.line_break();
        let s = &self.style;
        println!("  {} Approve {}?", s.yellow("?"), s.bold(tool.name()));
        for line in preview(args).lines() {
            println!("    {}", s.dim(line));
        }
        loop {
            print!("  {} ", s.yellow("[y]es / [n]o / [a]lways for this session ›"));
            let _ = io::stdout().flush();
            let mut answer = String::new();
            let read = io::stdin().read_line(&mut answer).unwrap_or(0);
            self.answered_at = Some(Instant::now());
            if read == 0 {
                return false;
            }
            match answer.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => return true,
                "n" | "no" | "" => return false,
                "a" | "always" => {
                    self.always.insert(tool.name().to_string());
                    return true;
                }
                _ => {}
            }
        }
    }
}

impl Default for Terminal {
    fn default() -> Self {
        Self::new()
    }
}

impl Ui for Terminal {
    fn text(&mut self, text: &str) {
        if self.last == Last::Thinking {
            println!();
        }
        print!("{text}");
        let _ = io::stdout().flush();
        self.last = if text.ends_with('\n') { Last::Line } else { Last::Text };
    }

    fn thinking(&mut self, text: &str) {
        if self.last == Last::Text {
            println!();
        }
        print!("{}", self.style.dim(text));
        let _ = io::stdout().flush();
        self.last = if text.ends_with('\n') { Last::Line } else { Last::Thinking };
    }

    fn tool_start(&mut self, name: &str, input: &Value) {
        self.line_break();
        let args = summary(input, 100);
        if args.is_empty() {
            println!("  {} {name}", self.style.cyan("⚙"));
        } else {
            println!("  {} {name} {}", self.style.cyan("⚙"), self.style.dim(&args));
        }
    }

    fn tool_end(&mut self, _name: &str, result: &Result<Value, String>, took: Duration) {
        let took = self.answered_at.take().map_or(took, |t| t.elapsed());
        let ms = format!("{}ms", took.as_millis());
        match result {
            Ok(_) => println!("    {} {}", self.style.green("✓"), self.style.dim(&ms)),
            Err(e) => println!("    {} {}", self.style.red("✗"), self.style.red(&truncate(e, 200))),
        }
    }

    fn notice(&mut self, text: &str) {
        self.line_break();
        println!("{}", self.style.yellow(text));
    }

    fn approve(&mut self, tool: &dyn Tool, args: &Value) -> bool {
        self.ask(tool, args)
    }

    fn route(&mut self, to: &str, model: &str, reason: &str) {
        self.line_break();
        let model = model.split(" · ").next().unwrap_or(model);
        let why = if reason.is_empty() { String::new() } else { format!(" · {reason}") };
        println!("{}", self.style.dim(&format!("→ {to} ({model}){why}")));
    }

    fn confirm(&mut self, question: &str) -> bool {
        self.line_break();
        print!("  {} {} ", self.style.yellow("?"), self.style.yellow(&format!("{question} [y/N] ›")));
        let _ = io::stdout().flush();
        let mut answer = String::new();
        let _ = io::stdin().read_line(&mut answer);
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }
}

/// Approver for one-off `orion call`s made from a terminal.
pub struct PromptApprover(std::cell::RefCell<Terminal>);

impl PromptApprover {
    /// `None` when stdin is not a terminal, so nobody can answer.
    pub fn new() -> Option<Self> {
        io::stdin().is_terminal().then(|| Self(std::cell::RefCell::new(Terminal::new())))
    }
}

impl Approver for PromptApprover {
    fn approve(&self, tool: &dyn Tool, args: &Value) -> bool {
        self.0.borrow_mut().ask(tool, args)
    }
}

/// Run the interactive chat until the user leaves. `mode` and `model`
/// override the `[chat]` settings.
pub fn run(orion: &Orion, mode: Option<Mode>, model: Option<&str>) -> Result<()> {
    let mut session = Session::start(orion, mode.unwrap_or(orion.policy().chat.mode), model)?;
    let mut term = Terminal::new();
    let mut editor = DefaultEditor::new()?;
    let history = orion.policy().audit_path().and_then(|p| Some(p.parent()?.join("chat_history")));
    if let Some(h) = &history {
        let _ = editor.load_history(h);
    }

    banner(&term.style, orion, &session);
    let prompt = if term.style.on { "\x1b[1;35morion ›\x1b[0m ".to_string() } else { "orion › ".to_string() };
    loop {
        let line = match editor.readline(&prompt) {
            Ok(line) => line,
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        };
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(input);
        if let Some(cmd) = input.strip_prefix('/') {
            if !command(cmd, orion, &mut session, &term.style) {
                break;
            }
            continue;
        }
        println!();
        if let Err(e) = session.turn(input, &mut term) {
            term.line_break();
            println!("{}", term.style.red(&format!("error: {e:#}")));
        }
        term.line_break();
        println!();
    }
    if let Some(h) = &history {
        if let Some(dir) = h.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = editor.save_history(h);
    }
    Ok(())
}

fn routing(session: &Session) -> String {
    match (session.mode, session.classifier_name()) {
        (Mode::Auto, Some(judge)) => format!("routing: auto (private → local; {judge} judges the rest)"),
        (Mode::Auto, None) => "routing: auto (private and simple → local, complex → Claude)".into(),
        (Mode::Claude, _) => "routing: always Claude".into(),
        (Mode::Local, _) => "routing: always local".into(),
    }
}

/// One line per provider: its model, or why it is missing.
fn providers(s: &Style, session: &Session) -> Vec<String> {
    [(Provider::Claude, "claude"), (Provider::Local, "local ")]
        .into_iter()
        .filter_map(|(p, label)| match (session.backend(p), session.backend_error(p)) {
            (Some(b), _) => Some(format!("  {label}  {}", b.describe())),
            (None, Some(e)) => Some(format!("  {label}  {}", s.red(&format!("unavailable: {}", e.lines().next().unwrap_or(e))))),
            (None, None) => None,
        })
        .collect()
}

fn banner(s: &Style, orion: &Orion, session: &Session) {
    let count = |d: Decision| orion.tools().filter(|(_, x)| *x == d).count();
    println!("{}", s.bold("Orion"));
    for line in providers(s, session) {
        println!("{}", s.dim(&line));
    }
    let routing = routing(session);
    println!(
        "{}",
        s.dim(&format!(
            "{routing} · {} tools ({} automatic, {} ask first) · /help for commands",
            count(Decision::Allow) + count(Decision::Ask),
            count(Decision::Allow),
            count(Decision::Ask),
        ))
    );
    println!();
}

/// Handle a slash command; `false` means quit.
fn command(cmd: &str, orion: &Orion, session: &mut Session, s: &Style) -> bool {
    let (name, arg) = cmd.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((cmd, ""));
    // `/model local qwen3:8b` names the provider; `/model qwen3:8b` means the current one.
    let (target, rest) = match arg.split_once(' ').unwrap_or((arg, "")) {
        ("claude", rest) => (Provider::Claude, rest.trim()),
        ("local", rest) => (Provider::Local, rest.trim()),
        _ => (session.current(), arg),
    };
    let missing = |p: Provider| println!("  {}", s.red(&format!("no {} backend connected; try /use {}", router::name(p), match p {
        Provider::Claude => "claude",
        Provider::Local => "local",
    })));
    match name {
        "exit" | "quit" | "q" => return false,
        "help" | "?" => {
            println!("  @claude … / @local …    send one message to that model");
            println!("  /use auto|claude|local  route automatically, or pin a model (the conversation carries over)");
            println!("  /route <text>           show where a message would go (asks only the local judge)");
            println!("  /model [claude|local] [id]  show or switch a model");
            println!("  /models                 list the models the local server has");
            println!("  /effort <level>         set Claude's effort: low, medium, high, xhigh, max");
            println!("  /tools                  what the AI can do, and what needs your approval");
            println!("  /usage                  tokens used this session");
            println!("  /clear                  start a new conversation");
            println!("  /exit                   leave (Ctrl-D works too)");
        }
        "tools" => {
            for (tool, decision) in orion.tools() {
                let d = match decision {
                    Decision::Allow => s.green("allow"),
                    Decision::Ask => s.yellow("ask  "),
                    Decision::Deny => s.red("deny "),
                };
                println!("  {d}  {:<12} {}", tool.name(), s.dim(tool.risk().as_str()));
            }
        }
        "model" if rest.is_empty() => {
            println!("  {}", s.dim(&routing(session)));
            providers(s, session).iter().for_each(|l| println!("{l}"));
        }
        "model" => match session.backend_mut(target) {
            Some(b) => {
                b.set_model(rest);
                println!("  {}", b.describe());
            }
            None => missing(target),
        },
        "effort" => match session.backend_mut(Provider::Claude) {
            Some(b) => match b.set_effort(arg) {
                Ok(()) => println!("  {}", b.describe()),
                Err(e) => println!("  {}", s.red(&e)),
            },
            None => missing(Provider::Claude),
        },
        "models" => match session.backend(Provider::Local) {
            Some(b) => match b.list_models() {
                Ok(models) if models.is_empty() => println!("  {}", s.dim("no models installed")),
                Ok(models) => models.iter().for_each(|m| println!("  {m}")),
                Err(e) => println!("  {}", s.red(&format!("{e:#}"))),
            },
            None => missing(Provider::Local),
        },
        "use" => {
            let mode = match arg {
                "auto" => Mode::Auto,
                "claude" => Mode::Claude,
                "local" => Mode::Local,
                _ => {
                    println!("  {}", s.red("usage: /use auto | claude | local"));
                    return true;
                }
            };
            match session.set_mode(mode) {
                Ok(()) => {
                    println!("  {}", s.dim(&routing(session)));
                    providers(s, session).iter().for_each(|l| println!("{l}"));
                }
                Err(e) => println!("  {}", s.red(&format!("{e:#}"))),
            }
        }
        "route" if arg.is_empty() => println!("  {}", s.red("usage: /route <message>")),
        "route" => println!("  → {}", session.explain_route(arg)),
        "usage" => {
            for (p, label) in [(Provider::Claude, "claude"), (Provider::Local, "local")] {
                let Some(b) = session.backend(p) else { continue };
                let u = b.usage();
                let mut line = format!("  {label:<7} input {} · output {}", u.input_tokens, u.output_tokens);
                if u.cache_read_input_tokens + u.cache_creation_input_tokens > 0 {
                    line += &format!(" · cache read {} · cache write {}", u.cache_read_input_tokens, u.cache_creation_input_tokens);
                }
                println!("{line}");
            }
        }
        "clear" => {
            session.clear();
            println!("  {}", s.dim("new conversation"));
        }
        _ => println!("  {}", s.red(&format!("unknown command /{cmd}; try /help"))),
    }
    true
}

/// Arguments for an approval prompt: pretty JSON with long strings shortened.
fn preview(args: &Value) -> String {
    fn shorten(v: &Value) -> Value {
        match v {
            Value::String(s) if s.len() > 400 => Value::String(format!("{}… ({} bytes)", truncate(s, 400), s.len())),
            Value::Array(a) => Value::Array(a.iter().map(shorten).collect()),
            Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), shorten(v))).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string_pretty(&shorten(args)).unwrap_or_default()
}

fn summary(input: &Value, max: usize) -> String {
    match input {
        Value::Object(o) if o.is_empty() => String::new(),
        _ => truncate(&input.to_string(), max),
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}
