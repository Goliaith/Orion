//! The interactive terminal: line editing, hotkeys, streamed output, tool
//! activity and approval prompts.

use std::collections::HashSet;
use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rustyline::error::ReadlineError;
use rustyline::{
    Cmd, ConditionalEventHandler, DefaultEditor, Event, EventContext, EventHandler, KeyCode, KeyEvent, Modifiers,
    RepeatCount,
};
use serde_json::Value;

use super::store::{self, Store, Turn};
use super::{router, Mode, Provider, Session, Ui};
use crate::{interrupt, Approver, Decision, Orion, Tool};

const HOTKEYS: &[(&str, &str)] = &[
    ("Ctrl-C", "stop the AI mid-turn (twice at the prompt: quit)"),
    ("Shift-Tab", "cycle routing: auto → claude → local"),
    ("Ctrl-O", "show or hide reasoning and progress notes"),
    ("Alt-Enter", "new line in your message"),
    ("Ctrl-R / ↑ ↓", "search / browse your earlier messages"),
    ("Ctrl-L", "clear the screen"),
    ("Ctrl-D", "quit"),
];

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
    /// Ctrl-O toggles this.
    show_reasoning: bool,
    /// Whether the "reasoning hidden" hint was shown this turn.
    hinted: bool,
}

impl Terminal {
    pub fn new() -> Self {
        let on = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        Self {
            style: Style { on },
            last: Last::Line,
            always: HashSet::new(),
            answered_at: None,
            show_reasoning: true,
            hinted: false,
        }
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
            // Ctrl-C (or EOF) here means no.
            let answer = interrupt::read_answer();
            self.answered_at = Some(Instant::now());
            let Some(answer) = answer else {
                println!();
                return false;
            };
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
        if !self.show_reasoning {
            if !self.hinted {
                self.hinted = true;
                self.line_break();
                println!("{}", self.style.dim("  … reasoning hidden (Ctrl-O shows it)"));
            }
            return;
        }
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
        let answer = interrupt::read_answer().unwrap_or_default();
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

/// What a hotkey asked for, with the line being edited so it can be put back.
struct Pending {
    action: Hotkey,
    line: String,
    pos: usize,
}

#[derive(Clone, Copy)]
enum Hotkey {
    CycleMode,
    ToggleReasoning,
}

/// Rustyline handler for one hotkey: note the request and the line, and end
/// this `readline` so the loop can act and restore the line.
struct HotkeyHandler {
    action: Hotkey,
    pending: Arc<Mutex<Option<Pending>>>,
}

impl ConditionalEventHandler for HotkeyHandler {
    fn handle(&self, _evt: &Event, _n: RepeatCount, _positive: bool, ctx: &EventContext) -> Option<Cmd> {
        let p = Pending { action: self.action, line: ctx.line().to_string(), pos: ctx.pos() };
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
        Some(Cmd::Interrupt)
    }
}

fn bind_hotkeys(editor: &mut DefaultEditor) -> Arc<Mutex<Option<Pending>>> {
    let pending = Arc::new(Mutex::new(None));
    let handler = |action| {
        EventHandler::Conditional(Box::new(HotkeyHandler { action, pending: pending.clone() }))
    };
    editor.bind_sequence(KeyEvent(KeyCode::BackTab, Modifiers::NONE), handler(Hotkey::CycleMode));
    editor.bind_sequence(KeyEvent::ctrl('O'), handler(Hotkey::ToggleReasoning));
    editor.bind_sequence(KeyEvent(KeyCode::Enter, Modifiers::ALT), EventHandler::Simple(Cmd::Newline));
    pending
}

fn next_mode(mode: Mode) -> Mode {
    match mode {
        Mode::Auto => Mode::Claude,
        Mode::Claude => Mode::Local,
        Mode::Local => Mode::Auto,
    }
}

fn prompt(style: &Style, mode: Mode) -> String {
    let mode = match mode {
        Mode::Auto => "auto",
        Mode::Claude => "claude",
        Mode::Local => "local",
    };
    if style.on {
        format!("\x1b[1;35morion\x1b[0m \x1b[2m{mode}\x1b[0m \x1b[1;35m›\x1b[0m ")
    } else {
        format!("orion {mode} › ")
    }
}

/// Run the interactive chat until the user leaves. `mode` and `model`
/// override the `[chat]` settings; `resume` continues a saved conversation
/// (by id or id prefix; empty for the latest).
pub fn run(orion: &Orion, mode: Option<Mode>, model: Option<&str>, resume: Option<&str>) -> Result<()> {
    let store = Store::default_location();
    let saved = match (resume, &store) {
        (Some(q), Some(store)) => Some(store.load(q)?),
        (Some(_), None) => anyhow::bail!("no state directory for saved conversations (is HOME set?)"),
        (None, _) => None,
    };
    // A resumed conversation goes back to its own mode unless one is given.
    let start_mode = mode.or(saved.as_ref().map(|c| c.mode)).unwrap_or(orion.policy().chat.mode);
    let resumed = saved.is_some();
    let mut session = match Session::start(orion, start_mode, model) {
        // The saved mode's model may be gone; the conversation can still go on in auto.
        Err(e) if resumed && mode.is_none() && start_mode != Mode::Auto => {
            let why = format!("{e:#}");
            println!("(can't use the saved mode: {}; using auto)", why.lines().next().unwrap_or_default());
            Session::start(orion, Mode::Auto, None)?
        }
        other => other?,
    };
    if let Some(conv) = saved {
        session.restore(conv);
        if let Some(m) = model {
            // --model wins over the model the conversation was saved with.
            if let Some(b) = session.backend_mut(session.current()) {
                b.set_model(m);
            }
        }
    }
    let saver = Saver { store: store.filter(|_| orion.policy().chat.save_conversations) };
    let mut term = Terminal::new();
    term.show_reasoning = orion.policy().chat.show_reasoning;
    let mut editor = DefaultEditor::new()?;
    let pending = bind_hotkeys(&mut editor);
    let history = orion.policy().audit_path().and_then(|p| Some(p.parent()?.join("chat_history")));
    if let Some(h) = &history {
        let _ = editor.load_history(h);
    }
    // From here on Ctrl-C stops the AI instead of killing Orion.
    interrupt::install();

    banner(&term.style, orion, &session);
    if resumed {
        recap(&term.style, &session);
    }
    let mut restore: Option<(String, usize)> = None;
    let mut last_ctrl_c: Option<Instant> = None;
    loop {
        let prompt = prompt(&term.style, session.mode);
        let line = match restore.take() {
            Some((line, pos)) => editor.readline_with_initial(&prompt, (&line[..pos], &line[pos..])),
            None => editor.readline(&prompt),
        };
        let line = match line {
            Ok(line) => line,
            Err(ReadlineError::Interrupted) => {
                if let Some(p) = pending.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    hotkey(p.action, &mut session, &mut term);
                    restore = Some((p.line, p.pos));
                    continue;
                }
                if last_ctrl_c.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                    break;
                }
                last_ctrl_c = Some(Instant::now());
                println!("{}", term.style.dim("  (Ctrl-C again to quit, or Ctrl-D)"));
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        };
        last_ctrl_c = None;
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(input);
        if let Some(cmd) = input.strip_prefix('/') {
            if !command(cmd, orion, &mut session, &mut term, &saver) {
                break;
            }
            continue;
        }
        println!();
        term.hinted = false;
        if let Err(e) = session.turn(input, &mut term) {
            term.line_break();
            println!("{}", term.style.red(&format!("error: {e:#}")));
        }
        interrupt::clear();
        saver.save(&session, &term.style);
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

/// Saves the conversation after each change, if saving is on.
struct Saver {
    store: Option<Store>,
}

impl Saver {
    fn save(&self, session: &Session, s: &Style) {
        let (Some(store), Some(conv)) = (&self.store, session.snapshot()) else { return };
        if let Err(e) = store.save(&conv) {
            println!("{}", s.red(&format!("  could not save the conversation: {e:#}")));
        }
    }
}

fn provider_label(p: Provider) -> &'static str {
    match p {
        Provider::Claude => "claude",
        Provider::Local => "local",
    }
}

fn clip(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max - 1).collect::<String>())
    }
}

/// What a resumed conversation was about: its title and the last exchange.
fn recap(s: &Style, session: &Session) {
    let turns = session.turns();
    println!(
        "{}",
        s.bold(&format!("Resumed “{}” · {} message{}", session.title(), turns.len(), if turns.len() == 1 { "" } else { "s" }))
    );
    for t in turns.iter().rev().take(2).rev() {
        print_turn(s, t, 160);
    }
    println!();
}

fn print_turn(s: &Style, t: &Turn, max: usize) {
    println!("  {} {}", s.dim("you:"), clip(&t.user, max));
    for tool in &t.tools {
        println!("    {} {}", s.cyan("⚙"), s.dim(&clip(tool, max)));
    }
    let reply = if t.reply.trim().is_empty() { "(no reply)" } else { &t.reply };
    println!("  {} {}", s.dim(&format!("{}:", provider_label(t.provider))), clip(reply, max * 2));
}

/// `orion conversations`: list, show or delete saved conversations.
pub fn conversations(action: Option<(&str, &str)>) -> Result<()> {
    let store = Store::default_location().context("no state directory (is HOME set?)")?;
    let s = Style { on: io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() };
    match action {
        None | Some(("list", _)) => {
            let all = store.list()?;
            if all.is_empty() {
                println!("no saved conversations ({})", store.dir().display());
            }
            for c in all {
                println!("{}  {:<14} {:>3} msg  {}", c.id, s.dim(&store::ago(c.updated_ms)), c.turns, c.title);
            }
        }
        Some(("show", id)) => {
            let c = store.load(id)?;
            println!("{}", s.bold(&c.title));
            println!(
                "{}",
                s.dim(&format!("{} · started {} · {} messages", c.id, store::local_time(c.created_ms, "%Y-%m-%d %H:%M"), c.turns.len()))
            );
            for t in &c.turns {
                println!();
                print_turn(&s, t, 10_000);
            }
        }
        Some(("delete", id)) => println!("deleted {}", store.delete(id)?),
        Some((other, _)) => anyhow::bail!("unknown action {other}"),
    }
    Ok(())
}

fn hotkey(action: Hotkey, session: &mut Session, term: &mut Terminal) {
    let s = &term.style;
    match action {
        Hotkey::CycleMode => {
            // Skip modes whose model can't be reached.
            let mut mode = next_mode(session.mode);
            let mut errors = Vec::new();
            for _ in 0..3 {
                match session.set_mode(mode) {
                    Ok(()) => break,
                    Err(e) => {
                        errors.push(format!("{mode:?}: {}", format!("{e:#}").lines().next().unwrap_or_default()));
                        mode = next_mode(mode);
                    }
                }
            }
            for e in errors {
                println!("  {}", s.red(&format!("skipped {}", e.to_lowercase())));
            }
            println!("  {}", s.dim(&routing(session)));
        }
        Hotkey::ToggleReasoning => {
            term.show_reasoning = !term.show_reasoning;
            let state = if term.show_reasoning { "shown" } else { "hidden" };
            println!("  {}", s.dim(&format!("reasoning {state}")));
        }
    }
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
            "{routing} · {} tools ({} automatic, {} ask first) · /help · Shift-Tab switches routing",
            count(Decision::Allow) + count(Decision::Ask),
            count(Decision::Allow),
            count(Decision::Ask),
        ))
    );
    println!();
}

/// Handle a slash command; `false` means quit.
fn command(cmd: &str, orion: &Orion, session: &mut Session, term: &mut Terminal, saver: &Saver) -> bool {
    let s = &term.style;
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
            println!("  /new                    start a new conversation (this one stays saved)");
            println!("  /history                saved conversations, most recent first");
            println!("  /resume <n|id>          continue a saved conversation");
            println!("  /title <text>           rename this conversation");
            println!("  /exit                   leave (Ctrl-D works too)");
            println!();
            for (key, what) in HOTKEYS {
                println!("  {key:<23} {what}");
            }
        }
        "keys" => {
            for (key, what) in HOTKEYS {
                println!("  {key:<14} {what}");
            }
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
        "clear" | "new" => {
            session.clear();
            println!("  {}", s.dim("new conversation"));
        }
        "title" if arg.is_empty() => println!("  {}", session.title()),
        "title" => {
            session.set_title(arg);
            saver.save(session, s);
            println!("  {}", s.dim(&format!("title → {}", session.title())));
        }
        "history" => match &saver.store {
            None => println!("  {}", s.dim("saving is off (chat.save_conversations)")),
            Some(store) => match store.list() {
                Ok(all) if all.is_empty() => println!("  {}", s.dim("no saved conversations yet")),
                Ok(all) => {
                    for (i, c) in all.iter().take(20).enumerate() {
                        let here = if c.id == session.id() { s.green(" ← this one") } else { String::new() };
                        println!("  {:>2}. {:<44} {}{here}", i + 1, clip(&c.title, 44), s.dim(&format!("{} · {} msg", store::ago(c.updated_ms), c.turns)));
                    }
                    println!("  {}", s.dim("/resume <number> continues one"));
                }
                Err(e) => println!("  {}", s.red(&format!("{e:#}"))),
            },
        },
        "resume" => {
            let Some(store) = &saver.store else {
                println!("  {}", s.dim("saving is off (chat.save_conversations)"));
                return true;
            };
            // A number picks from /history; anything else is an id (prefix).
            let query = match arg.parse::<usize>() {
                Ok(n) if n >= 1 => store.list().ok().and_then(|all| all.get(n - 1).map(|c| c.id.clone())),
                _ => Some(arg.to_string()),
            };
            match query.map(|q| store.load(&q)) {
                Some(Ok(conv)) => {
                    session.restore(conv);
                    recap(s, session);
                }
                Some(Err(e)) => println!("  {}", s.red(&format!("{e:#}"))),
                None => println!("  {}", s.red("no conversation with that number; see /history")),
            }
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
