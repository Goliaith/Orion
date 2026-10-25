//! Headless voice: `orion listen`. No screen and no keyboard: Orion waits
//! for its wake phrase, hears the request, works on it, and speaks the
//! answer. Made to run as a service (see contrib/orion-voice.service).
//!
//! Everything goes through the same conversation thread as the full-screen
//! chat and the shell, so routing, tools, the policy and saved
//! conversations work the same. What would be on a screen goes to the log
//! (stdout, so the journal when it runs as a service).

use std::io::Write;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde_json::Value;

use super::agent::{self, Answer, FromAgent, ToAgent};
use super::terminal::{prepare, Saver};
use super::{Mode, Session};
use crate::voice::{is_stop_request, spoken_answer, VoiceConfig, VoiceIo, WakeEvent};
use crate::{interrupt, Orion};

/// How long a spoken approval question waits for "Orion, yes" or "no".
const APPROVAL_WAIT: Duration = Duration::from_secs(30);
/// After the microphone fails, try again this often.
const RETRY: Duration = Duration::from_secs(10);

/// A question asked aloud, waiting for a spoken answer.
enum Asking {
    Approve(Sender<Answer>),
    Confirm(Sender<bool>),
}

/// Something to say, through whatever logs the service.
pub trait Log {
    fn line(&mut self, text: &str);
}

struct Stdout;

impl Log for Stdout {
    fn line(&mut self, text: &str) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{text}");
        let _ = out.flush();
    }
}

/// The headless interface: what it heard, what the conversation said, and
/// what to say back.
pub struct Headless<L: Log> {
    voice: VoiceIo,
    to_agent: Sender<ToAgent>,
    log: L,
    /// Turns and commands sent and not yet done.
    in_flight: usize,
    queued: Option<(String, Option<String>)>,
    asking: Option<(Asking, Instant)>,
    reply: String,
    last_done: Option<Instant>,
    /// Turns in the current conversation.
    turns: usize,
    failed_at: Option<Instant>,
    quit: bool,
}

impl<L: Log> Headless<L> {
    pub fn new(voice: VoiceIo, to_agent: Sender<ToAgent>, log: L) -> Self {
        Self {
            voice,
            to_agent,
            log,
            in_flight: 0,
            queued: None,
            asking: None,
            reply: String::new(),
            last_done: None,
            turns: 0,
            failed_at: None,
            quit: false,
        }
    }

    fn busy(&self) -> bool {
        self.in_flight > 0
    }

    fn say(&mut self, text: &str) {
        if let Ok(s) = &self.voice.speaker {
            s.say(text);
        }
    }

    fn send(&mut self, msg: ToAgent) {
        self.in_flight += 1;
        let _ = self.to_agent.send(msg);
    }

    /// Ask the conversation, starting a new one after a long quiet spell.
    fn ask(&mut self, text: String, speaker: Option<String>) {
        let mins = self.voice.config.listen_new_after_mins;
        let stale = self.last_done.is_some_and(|t| mins > 0 && t.elapsed() >= Duration::from_secs(mins * 60));
        if stale && self.turns > 0 {
            self.log.line("(a new conversation: the last one went quiet)");
            self.send(ToAgent::Command("new".into()));
            self.turns = 0;
        }
        self.log.line(&format!("{}: {text}", speaker.as_deref().unwrap_or("you")));
        self.turns += 1;
        match speaker {
            Some(name) => {
                let by = self.voice.config.who(&name);
                self.send(ToAgent::Spoken { text, by })
            }
            None => self.send(ToAgent::Turn(text)),
        }
    }

    pub fn on_wake(&mut self, ev: WakeEvent) {
        match ev {
            WakeEvent::Woke => {
                self.voice.hush();
                self.log.line("(woke: listening for the request)");
            }
            WakeEvent::NoRequest => self.log.line("(woke, but heard no request)"),
            WakeEvent::NotYou => {
                self.log.line("(a voice other than the enrolled one: ignored)");
                self.say("Sorry, I didn't recognize your voice.");
            }
            WakeEvent::Failed(e) => {
                self.log.line(&format!("the microphone stopped: {e} (trying again in {}s)", RETRY.as_secs()));
                self.failed_at = Some(Instant::now());
            }
            WakeEvent::Request { text, speaker } => {
                self.voice.hush();
                if let Some((asking, at)) = self.asking.take() {
                    return self.answer(asking, at, &text, speaker.as_deref());
                }
                if is_stop_request(&text) {
                    self.log.line(&format!("{}: {text}", speaker.as_deref().unwrap_or("you")));
                    if self.busy() {
                        interrupt::request();
                        self.queued = None;
                    }
                } else if self.busy() {
                    self.log.line(&format!("(queued: {text})"));
                    self.queued = Some((text, speaker));
                    self.say("I'll do that next.");
                } else {
                    self.ask(text, speaker);
                }
            }
        }
    }

    /// A spoken answer to the question asked aloud.
    fn answer(&mut self, asking: Asking, asked_at: Instant, text: &str, speaker: Option<&str>) {
        let yes = match spoken_answer(text) {
            Some(yes) => yes,
            None if is_stop_request(text) => false,
            None => {
                // Not an answer: ask again, and keep waiting.
                self.say("Please say Orion yes, or Orion no.");
                self.asking = Some((asking, Instant::now()));
                return;
            }
        };
        if yes && !self.voice.config.may_approve(speaker) {
            // Someone else may still answer, until the question times out.
            let name = speaker.unwrap_or_default();
            self.log.line(&format!("{name}: {text} (may not approve: voice.people.{name})"));
            self.say(&format!("Sorry, {}, you can't approve that.", super::agent::capitalize(name)));
            self.asking = Some((asking, asked_at));
            return;
        }
        self.log.line(&format!("{}: {text} ({})", speaker.unwrap_or("you"), if yes { "approved" } else { "declined" }));
        match asking {
            Asking::Approve(tx) => {
                let _ = tx.send(if yes { Answer::Yes } else { Answer::No });
            }
            Asking::Confirm(tx) => {
                let _ = tx.send(yes);
            }
        }
        if !yes {
            self.say("Okay, I won't.");
        }
    }

    /// Ask aloud, or decline at once if spoken approval is off.
    fn put_question(&mut self, asking: Asking, question: &str) {
        if !self.voice.config.wake_approvals {
            self.log.line(&format!("(declined, since spoken approval is off: {question})"));
            self.say("That needs your approval, and approving by voice is turned off, so I didn't.");
            match asking {
                Asking::Approve(tx) => drop(tx.send(Answer::No)),
                Asking::Confirm(tx) => drop(tx.send(false)),
            }
            return;
        }
        self.log.line(&format!("asking: {question}"));
        let phrase = self.voice.config.wake_phrase.trim().to_string();
        self.say(&format!("{question} Say {phrase} yes, or {phrase} no."));
        self.asking = Some((asking, Instant::now()));
    }

    pub fn on_agent(&mut self, ev: FromAgent) {
        match ev {
            FromAgent::Text(t) => {
                self.voice.reply_text(&t);
                self.reply.push_str(&t);
            }
            FromAgent::Thinking(_) | FromAgent::Transcript(_) | FromAgent::Status(_) => {}
            FromAgent::Route { to, model, reason } => self.log.line(&format!("→ {to} ({model}): {reason}")),
            FromAgent::ToolStart { name, args } => self.log.line(&format!("⚙ {name} {args}")),
            FromAgent::ToolEnd { ok, detail, took } => self.log.line(&if ok {
                format!("  ✓ {}ms", took.as_millis())
            } else {
                format!("  ✗ {detail}")
            }),
            FromAgent::Notice(t) | FromAgent::Output(t) => self.log.line(&t),
            FromAgent::Wait { text, label, secs } => {
                self.log.line(&text);
                // A long silence is worth explaining to someone who can't see the screen.
                if secs >= 5 {
                    self.say(&format!("One moment: {label}, about {secs} seconds."));
                }
            }
            FromAgent::Error(e) => {
                self.log.line(&format!("error: {e}"));
                let first = e.split(['.', '\n', ':']).next().unwrap_or_default().trim().to_string();
                self.say(&format!("Sorry, that failed: {first}."));
            }
            FromAgent::Approve { tool, preview, reply } => {
                let args: Value = serde_json::from_str(&preview).unwrap_or(Value::Null);
                if beyond_sandbox(&tool, &args) {
                    // Too much to approve by ear: the person should read it.
                    let _ = reply.send(Answer::No);
                    self.say("That needs approving on a screen, where you can read exactly what it does.");
                    return;
                }
                let question = format!("May I {}?", spoken_action(&tool, &args));
                self.put_question(Asking::Approve(reply), &question);
            }
            FromAgent::Confirm { question, reply } => self.put_question(Asking::Confirm(reply), &question),
            FromAgent::Done => {
                self.in_flight = self.in_flight.saturating_sub(1);
                self.voice.reply_done();
                if !self.reply.trim().is_empty() {
                    self.log.line(&format!("orion: {}", self.reply.trim()));
                    self.reply.clear();
                }
                if !self.busy() {
                    self.last_done = Some(Instant::now());
                    self.asking = None;
                    if let Some((text, speaker)) = self.queued.take() {
                        self.ask(text, speaker);
                    }
                }
            }
            FromAgent::Quit => self.quit = true,
        }
    }

    /// Time passing: unanswered questions, and getting the microphone back.
    pub fn tick(&mut self) {
        if self.asking.as_ref().is_some_and(|(_, at)| at.elapsed() >= APPROVAL_WAIT) {
            let (asking, _) = self.asking.take().expect("checked above");
            self.log.line("(no answer: declined)");
            self.say("No answer, so I didn't.");
            match asking {
                Asking::Approve(tx) => drop(tx.send(Answer::No)),
                Asking::Confirm(tx) => drop(tx.send(false)),
            }
        }
        if self.failed_at.is_some_and(|t| t.elapsed() >= RETRY) {
            self.failed_at = None;
            match self.voice.wake_on() {
                Ok(_) => self.log.line("listening again"),
                Err(e) => {
                    self.log.line(&format!("still can't listen: {e}"));
                    self.failed_at = Some(Instant::now());
                }
            }
        }
    }
}

/// A tool call as a short phrase: "run df -h", "install htop".
/// Root work and programs outside the sandbox: never approved by voice.
fn beyond_sandbox(tool: &str, args: &Value) -> bool {
    matches!(tool, "root_run" | "sys_edit") || (tool == "proc_run" && args["sandbox"] == Value::Bool(false))
}

fn spoken_action(tool: &str, args: &Value) -> String {
    let s = |k: &str| args[k].as_str().unwrap_or_default().to_string();
    let list = |k: &str| {
        args[k].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")).unwrap_or_default()
    };
    let unit = |u: String| u.trim_end_matches(".service").to_string();
    match tool {
        "proc_run" => format!("run {} {}", s("command"), list("args")).trim_end().to_string(),
        "fs_write" => format!("write to {}", s("path")),
        "svc_control" => format!("{} {}", s("action"), unit(s("unit"))),
        "pkg_install" => format!("install {}", list("packages")),
        "pkg_remove" => format!("remove {}", list("packages")),
        "flatpak_install" => format!("install {}", s("app")),
        "flatpak_remove" => format!("remove {}", s("app")),
        "model_load" => format!("load the model {}", s("model")),
        "model_unload" => format!("unload the model {}", s("model")),
        "model_pull" => format!("download the model {}", s("model")),
        other => format!("use {}", other.replace('_', " ")),
    }
}

/// Run headless until killed, or until a command quits.
pub fn run(orion: &Orion, mode: Option<Mode>, model: Option<&str>, resume: Option<&str>, quiet: bool) -> Result<()> {
    let config = VoiceConfig { wake: true, speak_replies: true, ..orion.policy().voice.clone() };
    if !config.enabled {
        bail!("voice is turned off in the policy (voice.enabled = false)");
    }
    let mut voice = VoiceIo::new(&config);
    let missing: Vec<String> = [
        voice.recorder.as_ref().err().map(|e| format!("the microphone: {e}")),
        voice.transcriber.as_ref().err().map(|e| format!("speech to text: {e}")),
        voice.speaker.as_ref().err().map(|e| format!("speech: {e}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !missing.is_empty() {
        bail!("headless voice needs a microphone, speech to text and speech; not set up:\n  {}", missing.join("\n  "));
    }
    voice.support_wake().map_err(|e| anyhow::anyhow!("can't listen for the wake phrase: {e}"))?;

    let (session, saver, _) = prepare(orion, mode, model, resume)?;
    let mut log = Stdout;
    log.line(&format!("Orion is listening: {}", voice.wake().map(|w| w.describe().to_string()).unwrap_or_default()));
    describe(&mut log, &session, &config);
    run_with(orion, session, saver, voice, log, quiet)
}

fn describe(log: &mut impl Log, session: &Session, config: &VoiceConfig) {
    for line in super::terminal::providers(&super::terminal::Style { on: false }, session) {
        log.line(line.trim());
    }
    log.line(&format!(
        "approvals by voice: {}",
        if config.wake_approvals { "on (\"Orion, yes\")" } else { "off: tools that ask are declined (voice.wake_approvals)" }
    ));
}

fn run_with(orion: &Orion, session: Session, saver: Saver, voice: VoiceIo, log: Stdout, quiet: bool) -> Result<()> {
    let (to_agent, agent_rx) = mpsc::channel();
    let (agent_tx, from_agent): (Sender<FromAgent>, Receiver<FromAgent>) = mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(move || agent::run(orion, session, saver, agent_rx, agent_tx));
        let mut h = Headless::new(voice, to_agent, log);
        // The conversation thread reports in once before any work.
        h.in_flight = 1;
        if !quiet {
            h.say("Listening.");
        }
        while !h.quit {
            while let Ok(ev) = from_agent.try_recv() {
                h.on_agent(ev);
            }
            while let Some(ev) = h.voice.wake_event() {
                h.on_wake(ev);
            }
            h.tick();
            if h.quit {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        // Dropping the sender ends the conversation thread.
        drop(h);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Lines(Arc<Mutex<Vec<String>>>);

    impl Log for Lines {
        fn line(&mut self, text: &str) {
            self.0.lock().unwrap().push(text.to_string());
        }
    }

    /// A headless interface whose speech goes to a file.
    fn headless(approvals: bool) -> (Headless<Lines>, Receiver<ToAgent>, tempfile::NamedTempFile, Lines) {
        let said = tempfile::NamedTempFile::new().unwrap();
        let config = VoiceConfig {
            speak_command: format!("cat >> {}; echo >> {}", said.path().display(), said.path().display()),
            wake_approvals: approvals,
            ..Default::default()
        };
        let mut voice = VoiceIo::new(&config);
        voice.speak_replies = true;
        let (tx, rx) = mpsc::channel();
        let lines = Lines::default();
        (Headless::new(voice, tx, lines.clone()), rx, said, lines)
    }

    fn spoken(f: &tempfile::NamedTempFile, want: &str) -> String {
        let start = Instant::now();
        loop {
            let text = std::fs::read_to_string(f.path()).unwrap();
            if text.contains(want) || start.elapsed() > Duration::from_secs(5) {
                return text;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_spoken_conversation() {
        let (mut h, rx, said, lines) = headless(false);
        h.on_wake(WakeEvent::Request { text: "how long has this machine been up?".into(), speaker: None });
        assert!(matches!(rx.try_recv().unwrap(), ToAgent::Turn(t) if t == "how long has this machine been up?"));
        // Asked while busy: queued, and sent when the first is done.
        h.on_wake(WakeEvent::Request { text: "and the load?".into(), speaker: None });
        assert!(rx.try_recv().is_err());
        h.on_agent(FromAgent::Text("Up three days. ".into()));
        h.on_agent(FromAgent::Text("All is well.".into()));
        h.on_agent(FromAgent::Done);
        assert!(matches!(rx.try_recv().unwrap(), ToAgent::Turn(t) if t == "and the load?"));
        assert!(spoken(&said, "All is well.").contains("Up three days."));
        let log = lines.0.lock().unwrap().join("\n");
        assert!(log.contains("orion: Up three days. All is well."), "{log}");

        // Tools that ask are declined aloud when spoken approval is off.
        let (reply, answer) = mpsc::channel();
        h.on_agent(FromAgent::Approve { tool: "proc_run".into(), preview: r#"{"command":"df"}"#.into(), reply });
        assert_eq!(answer.try_recv().unwrap(), Answer::No);
        assert!(spoken(&said, "approving by voice is turned off").contains("approving by voice is turned off"));
    }

    #[test]
    fn approves_by_voice_when_allowed() {
        let (mut h, _rx, said, _) = headless(true);
        let (reply, answer) = mpsc::channel();
        let preview = serde_json::to_string_pretty(&serde_json::json!({ "unit": "ollama.service", "action": "restart" })).unwrap();
        h.on_agent(FromAgent::Approve { tool: "svc_control".into(), preview, reply });
        assert!(spoken(&said, "May I restart ollama?").contains("Say orion yes, or orion no."));
        // Anything else is not an answer: it asks again and keeps waiting.
        h.on_wake(WakeEvent::Request { text: "what's the weather".into(), speaker: None });
        assert!(answer.try_recv().is_err());
        h.on_wake(WakeEvent::Request { text: "Yes.".into(), speaker: None });
        assert_eq!(answer.try_recv().unwrap(), Answer::Yes);

        let (reply, answer) = mpsc::channel();
        h.on_agent(FromAgent::Approve { tool: "pkg_install".into(), preview: r#"{"packages":["htop"]}"#.into(), reply });
        h.on_wake(WakeEvent::Request { text: "no".into(), speaker: None });
        assert_eq!(answer.try_recv().unwrap(), Answer::No);

        // Root work and runs outside the sandbox are for reading on a screen.
        for (tool, preview) in [("root_run", r#"{"command":"sysctl"}"#), ("sys_edit", r#"{"path":"/etc/x"}"#), ("proc_run", r#"{"command":"make","sandbox":false}"#)] {
            let (reply, answer) = mpsc::channel();
            h.on_agent(FromAgent::Approve { tool: tool.into(), preview: preview.into(), reply });
            assert_eq!(answer.try_recv().unwrap(), Answer::No, "{tool}");
        }
        assert!(spoken(&said, "on a screen").contains("approving on a screen"));
    }

    #[test]
    fn only_people_allowed_may_approve_and_requests_name_the_speaker() {
        let (mut h, rx, said, lines) = headless(true);
        h.voice.config.people.insert("kid".into(), crate::voice::PersonPolicy { approve: false, ..Default::default() });
        h.on_wake(WakeEvent::Request { text: "restart ollama".into(), speaker: Some("ana".into()) });
        assert!(matches!(rx.try_recv().unwrap(), ToAgent::Spoken { by, .. } if by.name == "ana"));
        let (reply, answer) = mpsc::channel();
        h.on_agent(FromAgent::Approve { tool: "svc_control".into(), preview: r#"{"unit":"ollama","action":"restart"}"#.into(), reply });
        h.on_wake(WakeEvent::Request { text: "yes".into(), speaker: Some("kid".into()) });
        assert!(answer.try_recv().is_err(), "the kid can't approve");
        assert!(spoken(&said, "Sorry, Kid").contains("Sorry, Kid, you can't approve that."));
        h.on_wake(WakeEvent::Request { text: "yes".into(), speaker: Some("ana".into()) });
        assert_eq!(answer.try_recv().unwrap(), Answer::Yes);
        assert!(lines.0.lock().unwrap().iter().any(|l| l == "ana: restart ollama"));
    }

    #[test]
    fn starts_afresh_after_a_quiet_spell() {
        let (mut h, rx, _said, _) = headless(false);
        h.voice.config.listen_new_after_mins = 1;
        h.on_wake(WakeEvent::Request { text: "hello".into(), speaker: None });
        h.on_agent(FromAgent::Done);
        h.last_done = Some(Instant::now() - Duration::from_secs(120));
        h.on_wake(WakeEvent::Request { text: "what time is it".into(), speaker: None });
        let sent: Vec<String> = rx
            .try_iter()
            .map(|m| match m {
                ToAgent::Turn(t) => t,
                ToAgent::Command(c) => format!("/{c}"),
                ToAgent::Spoken { text, by } => format!("{}: {text}", by.name),
                ToAgent::CycleMode => "cycle".into(),
            })
            .collect();
        assert_eq!(sent, ["hello", "/new", "what time is it"]);
        // Two in flight: the /new and the turn.
        assert_eq!(h.in_flight, 2);
    }

    #[test]
    fn says_what_a_tool_would_do() {
        let a = |tool, v: Value| spoken_action(tool, &v);
        assert_eq!(a("proc_run", serde_json::json!({ "command": "df", "args": ["-h"] })), "run df -h");
        assert_eq!(a("pkg_install", serde_json::json!({ "packages": ["htop", "jq"] })), "install htop jq");
        assert_eq!(a("gpu_info", Value::Null), "use gpu info");
    }
}
