//! The conversation's own thread, shared by the full-screen chat and the
//! graphical shell. The interface sends it work; it runs the session and
//! reports everything back as events, so the interface stays live while
//! the AI streams, thinks or runs tools.

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::store::Turn;
use super::terminal::{self, preview, providers, routing, summary, Saver, Style};
use super::{Mode, Provider, Session, Ui, Usage};
use crate::{interrupt, Decision, Orion, Tool};

pub enum ToAgent {
    Turn(String),
    /// A request spoken by an enrolled person: their name goes before it,
    /// and their limits hold for the turn.
    Spoken { text: String, by: crate::voice::Person },
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
    /// A rate limit or busy server being waited out: the whole notice,
    /// what is being waited on, and for how long.
    Wait { text: String, label: String, secs: u64 },
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
    pub id: String,
    pub title: String,
    pub turns: usize,
    pub tools_auto: usize,
    pub tools_ask: usize,
    /// Where tool calls run: "through the daemon (pid N)", or here.
    pub tools_run: String,
    /// Whose conversation it is, and how much is remembered about them.
    pub person: String,
    pub notes: Option<usize>,
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
        id: session.id().to_string(),
        title: session.title(),
        turns: session.turns().len(),
        tools_auto: count(Decision::Allow),
        tools_ask: count(Decision::Ask),
        tools_run: orion.runs_tools(),
        person: session.person().to_string(),
        notes: session.memory().map(|m| m.notes.len()),
    }
}

/// Forwards what the session shows to the screen thread.
struct ChannelUi<'a> {
    tx: &'a Sender<FromAgent>,
    always: &'a mut HashSet<String>,
    answered_at: Option<Instant>,
    /// Who spoke this turn's request, if it was spoken.
    speaker: Option<crate::voice::Person>,
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

    fn wait(&mut self, wait: &super::http::Wait) {
        self.send(FromAgent::Wait { text: wait.to_string(), label: wait.label(), secs: wait.secs });
    }

    fn approve(&mut self, tool: &dyn Tool, args: &Value) -> bool {
        if self.always.contains(tool.name()) && !tool.every_time(args) {
            return true;
        }
        let (reply, rx) = std::sync::mpsc::channel();
        self.send(FromAgent::Approve { tool: tool.name().to_string(), preview: preview(&tool.redact(args)), reply });
        let answer = Self::wait(rx, Answer::No);
        self.answered_at = Some(Instant::now());
        // Calls that ask every time are approved this once.
        if answer == Answer::Always && !tool.every_time(args) {
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

    fn refuses(&mut self, _tool: &str, risk: crate::tool::Risk) -> Option<String> {
        let s = self.speaker.as_ref()?;
        s.tools.refuses(&s.name, risk)
    }

    fn caller(&self) -> Option<String> {
        self.speaker.as_ref().map(|s| format!("{} (by voice)", s.name))
    }
}

pub use crate::voice::capitalize;

/// Save this conversation and go to `person`'s: their latest, if it was
/// active recently (`voice.listen_new_after_mins`), or a new one.
fn switch_person(orion: &Orion, session: &mut Session, saver: &Saver, person: &str) -> String {
    let mut out = String::new();
    saver.save(session, &Style { on: false }, &mut out);
    let mins = orion.policy().voice.listen_new_after_mins;
    let fresh = |updated_ms: u64| mins == 0 || super::store::now_ms().saturating_sub(updated_ms) <= mins * 60_000;
    let latest = saver
        .store
        .as_ref()
        .and_then(|s| s.latest_for(person, &super::session::owner_name()).ok().flatten())
        .filter(|c| fresh(c.updated_ms));
    let name = capitalize(person);
    match latest {
        Some(conv) => {
            session.restore(conv);
            format!("{name}'s conversation")
        }
        None => {
            session.clear();
            session.set_person(person);
            format!("a new conversation with {name}")
        }
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
            ToAgent::Turn(_) | ToAgent::Spoken { .. } => {
                let (text, speaker) = match msg {
                    ToAgent::Spoken { text, by } => (format!("{} (by voice): {text}", capitalize(&by.name)), Some(by)),
                    ToAgent::Turn(text) => (text, None),
                    _ => unreachable!(),
                };
                // Each person has their own conversations; typed messages
                // belong to whoever is logged in.
                let person = speaker.as_ref().map_or_else(super::session::owner_name, |s| s.name.clone());
                if session.person() != person {
                    let notice = switch_person(orion, &mut session, &saver, &person);
                    send(FromAgent::Transcript(session.turns().to_vec()));
                    send(FromAgent::Notice(notice));
                }
                interrupt::clear();
                let mut ui = ChannelUi { tx: &tx, always: &mut always, answered_at: None, speaker };
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

#[cfg(test)]
mod tests {
    use super::super::store::{Conversation, Store, Turn, FORMAT_VERSION};
    use super::super::test_support::orion;
    use super::*;

    fn saved(store: &Store, id: &str, owner: &str, said: &str) {
        let now = super::super::store::now_ms();
        let turn = Turn { user: said.into(), provider: Provider::Local, private: false, reply: "ok".into(), tools: vec![], direct: false };
        let conv = Conversation {
            version: FORMAT_VERSION,
            id: id.into(),
            title: said.into(),
            created_ms: now,
            updated_ms: now,
            mode: Mode::Local,
            previous: None,
            turns: vec![turn],
            backends: Default::default(),
            owner: owner.into(),
        };
        store.save(&conv).unwrap();
    }

    #[test]
    fn each_person_has_their_own_conversations() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        saved(&store, "20261020-090000-aaaa", "ana", "what's using the GPU?");
        let saver = Saver { store: Some(Store::new(dir.path())) };
        let orion = orion();
        let mut s = Session::with_backends(&orion, Mode::Local, None, None);
        s.set_memory_dir(None);
        assert_eq!(s.person(), super::super::session::owner_name());

        // Ana continues her own recent conversation.
        assert_eq!(switch_person(&orion, &mut s, &saver, "ana"), "Ana's conversation");
        assert_eq!(s.person(), "ana");
        assert_eq!(s.turns()[0].user, "what's using the GPU?");
        // Ben has none yet: a new one, and Ana's is left as it was.
        assert_eq!(switch_person(&orion, &mut s, &saver, "ben"), "a new conversation with Ben");
        assert!(s.turns().is_empty() && s.person() == "ben");
        assert_eq!(store.latest_for("ana", "x").unwrap().unwrap().turns.len(), 1);
        // Back to Ana: hers again.
        switch_person(&orion, &mut s, &saver, "ana");
        assert_eq!(s.turns().len(), 1);
    }
}
