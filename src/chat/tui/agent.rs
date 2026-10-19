//! The conversation's own thread. The screen thread sends it work; it runs
//! the session and reports everything back as events, so the screen keeps
//! drawing and reading keys while the AI streams, thinks or runs tools.

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::super::store::Turn;
use super::super::terminal::{self, preview, providers, routing, summary, Saver, Style};
use super::super::{Mode, Provider, Session, Ui, Usage};
use crate::{interrupt, Decision, Orion, Tool};

pub enum ToAgent {
    Turn(String),
    /// A slash command, without the slash.
    Command(String),
    CycleMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
    Always,
}

pub enum FromAgent {
    Text(String),
    Thinking(String),
    Route { to: String, model: String, reason: String },
    ToolStart { name: String, args: String },
    ToolEnd { ok: bool, detail: String, took: Duration },
    Notice(String),
    Error(String),
    /// A tool call needs approval; answer on `reply`.
    Approve { tool: String, preview: String, reply: Sender<Answer> },
    /// A yes/no question; answer on `reply`.
    Confirm { question: String, reply: Sender<bool> },
    /// What a command printed.
    Output(String),
    /// The whole transcript changed (start, resume, new conversation).
    Transcript(Vec<Turn>),
    Status(Status),
    /// The turn or command finished.
    Done,
    /// A command asked to leave.
    Quit,
}

/// What the side panel and status bar show about the session.
#[derive(Clone, Default)]
pub struct Status {
    pub mode: Mode,
    pub routing: String,
    /// One line per provider: its model, or why it is unavailable.
    pub providers: Vec<String>,
    pub usage: Vec<(&'static str, Usage)>,
    pub title: String,
    pub turns: usize,
    pub tools_auto: usize,
    pub tools_ask: usize,
}

pub fn status(orion: &Orion, session: &Session) -> Status {
    let plain = Style { on: false };
    let count = |d: Decision| orion.tools().filter(|(_, x)| *x == d).count();
    Status {
        mode: session.mode,
        routing: routing(session),
        providers: providers(&plain, session).into_iter().map(|l| l.trim().to_string()).collect(),
        usage: [(Provider::Claude, "claude"), (Provider::Local, "local")]
            .into_iter()
            .filter_map(|(p, label)| session.backend(p).map(|b| (label, b.usage())))
            .collect(),
        title: session.title(),
        turns: session.turns().len(),
        tools_auto: count(Decision::Allow),
        tools_ask: count(Decision::Ask),
    }
}

/// Forwards what the session shows to the screen thread.
struct ChannelUi<'a> {
    tx: &'a Sender<FromAgent>,
    always: &'a mut HashSet<String>,
    answered_at: Option<Instant>,
}

impl ChannelUi<'_> {
    fn send(&self, ev: FromAgent) {
        let _ = self.tx.send(ev);
    }

    /// Wait for an answer, giving up (as "no") on Ctrl-C or if the screen
    /// thread is gone.
    fn wait<T>(rx: Receiver<T>, no: T) -> T {
        loop {
            if interrupt::requested() {
                return no;
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(v) => return v,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return no,
            }
        }
    }
}

impl Ui for ChannelUi<'_> {
    fn text(&mut self, text: &str) {
        self.send(FromAgent::Text(text.to_string()));
    }

    fn thinking(&mut self, text: &str) {
        self.send(FromAgent::Thinking(text.to_string()));
    }

    fn tool_start(&mut self, name: &str, input: &Value) {
        self.send(FromAgent::ToolStart { name: name.to_string(), args: summary(input, 120) });
    }

    fn tool_end(&mut self, _name: &str, result: &Result<Value, String>, took: Duration) {
        // Leave out the time the user spent deciding.
        let took = self.answered_at.take().map_or(took, |t| t.elapsed());
        let (ok, detail) = match result {
            Ok(_) => (true, String::new()),
            Err(e) => (false, e.clone()),
        };
        self.send(FromAgent::ToolEnd { ok, detail, took });
    }

    fn notice(&mut self, text: &str) {
        self.send(FromAgent::Notice(text.to_string()));
    }

    fn approve(&mut self, tool: &dyn Tool, args: &Value) -> bool {
        if self.always.contains(tool.name()) {
            return true;
        }
        let (reply, rx) = std::sync::mpsc::channel();
        self.send(FromAgent::Approve { tool: tool.name().to_string(), preview: preview(args), reply });
        let answer = Self::wait(rx, Answer::No);
        self.answered_at = Some(Instant::now());
        if answer == Answer::Always {
            self.always.insert(tool.name().to_string());
        }
        answer != Answer::No
    }

    fn route(&mut self, to: &str, model: &str, reason: &str) {
        let model = model.split(" · ").next().unwrap_or(model).to_string();
        self.send(FromAgent::Route { to: to.to_string(), model, reason: reason.to_string() });
    }

    fn confirm(&mut self, question: &str) -> bool {
        let (reply, rx) = std::sync::mpsc::channel();
        self.send(FromAgent::Confirm { question: question.to_string(), reply });
        Self::wait(rx, false)
    }
}

/// Serve the screen thread until it hangs up or a command quits.
pub fn run(orion: &Orion, mut session: Session, saver: Saver, rx: Receiver<ToAgent>, tx: Sender<FromAgent>) {
    let plain = Style { on: false };
    let mut always = HashSet::new();
    let send = |ev| {
        let _ = tx.send(ev);
    };
    send(FromAgent::Transcript(session.turns().to_vec()));
    send(FromAgent::Status(status(orion, &session)));
    send(FromAgent::Done);
    for msg in rx {
        let mut out = String::new();
        match msg {
            ToAgent::Turn(text) => {
                interrupt::clear();
                let mut ui = ChannelUi { tx: &tx, always: &mut always, answered_at: None };
                if let Err(e) = session.turn(&text, &mut ui) {
                    send(FromAgent::Error(format!("{e:#}")));
                }
                interrupt::clear();
                saver.save(&session, &plain, &mut out);
            }
            ToAgent::Command(cmd) => {
                let before = session.id().to_string();
                let keep_going = terminal::command(&cmd, orion, &mut session, &plain, &saver, &mut out);
                if session.id() != before {
                    // Resumed or started a conversation: show it whole, not a recap.
                    out.clear();
                    send(FromAgent::Transcript(session.turns().to_vec()));
                }
                if !keep_going {
                    send(FromAgent::Quit);
                    return;
                }
            }
            ToAgent::CycleMode => out = terminal::cycle_mode(&mut session, &plain),
        }
        if !out.trim().is_empty() {
            send(FromAgent::Output(out.trim_end().to_string()));
        }
        send(FromAgent::Status(status(orion, &session)));
        send(FromAgent::Done);
    }
}
