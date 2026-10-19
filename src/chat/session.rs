//! One conversation across up to two models. Every user message is routed
//! to Claude or the local model (or pinned to one), and both share a single
//! transcript: before a model answers, it is caught up on the turns the
//! other one handled. Private turns are never shown to Claude.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use super::classifier::LocalClassifier;
use super::router::{self, Classifier, Route, RouteContext, Router, Strategy};
use super::{connect, Backend, Event, Mode, Provider, Stop, ToolCall, ToolResult, Ui, UiApprover};
use crate::Orion;

/// A finished turn, as the other model will be told about it.
struct TurnRecord {
    user: String,
    provider: Provider,
    /// Kept away from Claude.
    private: bool,
    reply: String,
    tools: Vec<String>,
}

impl TurnRecord {
    /// The turn as plain text for a model that didn't handle it.
    fn render(&self) -> String {
        let mut s = format!("[{} handled this message", capitalized(router::name(self.provider)));
        if self.tools.is_empty() {
            s.push_str(".]");
        } else {
            s.push_str(". Its tool calls:\n");
            for t in &self.tools {
                s.push_str(&format!("- {t}\n"));
            }
            s.push(']');
        }
        let reply = self.reply.trim();
        s.push_str("\n\n");
        s.push_str(if reply.is_empty() { "(no reply)" } else { reply });
        s
    }
}

fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// A backend, or why there isn't one.
#[derive(Default)]
struct Slot {
    backend: Option<Box<dyn Backend>>,
    error: Option<String>,
    /// How many transcript turns this backend has been told about.
    synced: usize,
}

pub struct Session<'a> {
    orion: &'a Orion,
    pub mode: Mode,
    router: Router,
    /// Judges complexity under `strategy = "model"`; needs the local server.
    classifier: Option<Box<dyn Classifier>>,
    claude: Slot,
    local: Slot,
    log: Vec<TurnRecord>,
    previous: Option<Provider>,
    max_tool_rounds: usize,
}

impl<'a> Session<'a> {
    /// Connect what `mode` needs. In `auto`, one backend failing is fine;
    /// both failing is not.
    pub fn start(orion: &'a Orion, mode: Mode, model: Option<&str>) -> Result<Self> {
        let mut s = Self::with_backends(orion, mode, None, None);
        let wanted: &[Provider] = match mode {
            Mode::Claude => &[Provider::Claude],
            Mode::Local => &[Provider::Local],
            Mode::Auto => &[Provider::Claude, Provider::Local],
        };
        if model.is_some() && mode == Mode::Auto {
            bail!("--model needs --claude or --local, so it is clear which model it replaces");
        }
        for &p in wanted {
            let slot = s.slot_mut(p);
            match connect(orion, p, model) {
                Ok(b) => slot.backend = Some(b),
                Err(e) => slot.error = Some(format!("{e:#}")),
            }
        }
        if s.claude.backend.is_none() && s.local.backend.is_none() {
            let errors: Vec<String> = [&s.claude.error, &s.local.error].into_iter().flatten().cloned().collect();
            bail!("{}", errors.join("\n"));
        }
        s.classifier = s.build_classifier();
        Ok(s)
    }

    fn build_classifier(&self) -> Option<Box<dyn Classifier>> {
        let chat = &self.orion.policy().chat;
        if chat.router.strategy != Strategy::Model {
            return None;
        }
        let local = self.local.backend.as_ref()?;
        let model = match chat.router.classifier_model.as_str() {
            "" => local.model(),
            m => m.to_string(),
        };
        let timeout = Duration::from_secs(chat.router.classifier_timeout_secs.max(1));
        let c = LocalClassifier::new(&chat.local, model, chat.router.prefer, timeout).ok()?;
        Some(Box::new(c))
    }

    /// Replace the classifier (tests use a scripted one).
    pub fn set_classifier(&mut self, classifier: Option<Box<dyn Classifier>>) {
        self.classifier = classifier;
    }

    /// The model judging complexity, while it is in use.
    pub fn classifier_name(&self) -> Option<String> {
        self.classifier.as_ref().filter(|_| self.router.uses_classifier()).map(|c| c.name())
    }

    pub fn with_backends(
        orion: &'a Orion,
        mode: Mode,
        claude: Option<Box<dyn Backend>>,
        local: Option<Box<dyn Backend>>,
    ) -> Self {
        let chat = &orion.policy().chat;
        Self {
            orion,
            mode,
            router: Router::new(chat.router.clone()),
            classifier: None,
            claude: Slot { backend: claude, ..Default::default() },
            local: Slot { backend: local, ..Default::default() },
            log: Vec::new(),
            previous: None,
            max_tool_rounds: chat.max_tool_rounds,
        }
    }

    fn slot(&self, p: Provider) -> &Slot {
        match p {
            Provider::Claude => &self.claude,
            Provider::Local => &self.local,
        }
    }

    fn slot_mut(&mut self, p: Provider) -> &mut Slot {
        match p {
            Provider::Claude => &mut self.claude,
            Provider::Local => &mut self.local,
        }
    }

    pub fn backend(&self, p: Provider) -> Option<&dyn Backend> {
        self.slot(p).backend.as_deref()
    }

    pub fn backend_mut(&mut self, p: Provider) -> Option<&mut Box<dyn Backend>> {
        self.slot_mut(p).backend.as_mut()
    }

    /// Why a backend could not be connected.
    pub fn backend_error(&self, p: Provider) -> Option<&str> {
        self.slot(p).error.as_deref()
    }

    /// The backend a bare `/model` or `/usage` means: the pinned one, else
    /// whichever answered last.
    pub fn current(&self) -> Provider {
        match self.mode {
            Mode::Claude => Provider::Claude,
            Mode::Local => Provider::Local,
            Mode::Auto => self.previous.unwrap_or(if self.claude.backend.is_some() { Provider::Claude } else { Provider::Local }),
        }
    }

    /// Pin to one provider or go back to `auto`, connecting what is needed.
    /// The conversation carries over.
    pub fn set_mode(&mut self, mode: Mode) -> Result<()> {
        let needed = match mode {
            Mode::Claude => vec![Provider::Claude],
            Mode::Local => vec![Provider::Local],
            Mode::Auto => vec![Provider::Claude, Provider::Local],
        };
        for p in needed {
            if self.slot(p).backend.is_none() {
                let connected = connect(self.orion, p, None);
                let slot = self.slot_mut(p);
                match connected {
                    Ok(b) => {
                        slot.backend = Some(b);
                        slot.error = None;
                    }
                    Err(e) if mode != Mode::Auto => return Err(e),
                    Err(e) => slot.error = Some(format!("{e:#}")),
                }
            }
        }
        if self.classifier.is_none() {
            self.classifier = self.build_classifier();
        }
        self.mode = mode;
        Ok(())
    }

    pub fn clear(&mut self) {
        for p in [Provider::Claude, Provider::Local] {
            let slot = self.slot_mut(p);
            if let Some(b) = slot.backend.as_mut() {
                b.clear();
            }
            slot.synced = 0;
        }
        self.log.clear();
        self.previous = None;
    }

    /// What the router would do with `text`, without sending it.
    pub fn explain_route(&self, text: &str) -> String {
        match self.decide(text) {
            Route::To { provider, reason } => format!("{} ({reason})", router::name(provider)),
            Route::PrivateButNoLocal { reason } => format!("would ask you first ({reason}; no local model)"),
        }
    }

    fn decide(&self, text: &str) -> Route {
        let ctx = RouteContext {
            previous: self.previous,
            previous_text: self.log.last().map(|r| r.user.as_str()),
            claude_available: self.claude.backend.is_some(),
            local_available: self.local.backend.is_some(),
        };
        self.router.route(text, &ctx, self.classifier.as_deref())
    }

    /// Send one user message and run tools until the model is done. A
    /// leading `@claude` or `@local` sends it to that model regardless of
    /// the mode.
    pub fn turn(&mut self, input: &str, ui: &mut dyn Ui) -> Result<()> {
        let (forced, text) = match input.split_once(char::is_whitespace) {
            Some(("@claude", rest)) => (Some(Provider::Claude), rest.trim()),
            Some(("@local", rest)) => (Some(Provider::Local), rest.trim()),
            _ => (None, input),
        };
        let private_reason = self.router.private_reason(text);
        let (provider, reason, routed) = match (forced, self.mode) {
            (Some(p), _) => (p, "you picked it".to_string(), false),
            (None, Mode::Claude) => (Provider::Claude, String::new(), false),
            (None, Mode::Local) => (Provider::Local, String::new(), false),
            (None, Mode::Auto) => match self.decide(text) {
                Route::To { provider, reason } => (provider, reason, true),
                Route::PrivateButNoLocal { reason } => {
                    if !ui.confirm(&format!("This looks private ({reason}) and no local model is available. Send it to Claude?")) {
                        ui.notice("Not sent.");
                        return Ok(());
                    }
                    (Provider::Claude, "you confirmed".to_string(), false)
                }
            },
        };
        if self.slot(provider).backend.is_none() {
            let why = self.slot(provider).error.clone().unwrap_or_else(|| "not connected".into());
            bail!("{} is not available: {why}", router::name(provider));
        }
        if self.mode == Mode::Auto || forced.is_some() {
            ui.route(router::name(provider), &self.describe(provider), &reason);
        }
        // Private only while it stays on this machine; sending it to Claude
        // anyway (on purpose) makes it ordinary history.
        let private = private_reason.is_some() && provider == Provider::Local;

        let (record, result) = self.run_on(provider, text, private, ui);
        let started = !record.reply.is_empty() || !record.tools.is_empty();
        self.finish(record);
        let Err(e) = result else { return Ok(()) };

        // In auto mode, a model that fails before saying anything hands the
        // message to the other one, but private messages never go to Claude.
        let other = router::other(provider);
        if !routed || started || self.slot(other).backend.is_none() || (private_reason.is_some() && other == Provider::Claude) {
            return Err(e);
        }
        ui.notice(&format!("{} failed ({e:#}); trying {}.", capitalized(router::name(provider)), router::name(other)));
        ui.route(router::name(other), &self.describe(other), "fallback");
        let (record, result) = self.run_on(other, text, private_reason.is_some(), ui);
        self.finish(record);
        result
    }

    fn describe(&self, p: Provider) -> String {
        self.backend(p).map(|b| b.describe()).unwrap_or_default()
    }

    fn finish(&mut self, record: TurnRecord) {
        let p = record.provider;
        self.log.push(record);
        self.slot_mut(p).synced = self.log.len();
        self.previous = Some(p);
    }

    /// Tell `provider`'s backend about turns the other model handled.
    fn sync(&mut self, provider: Provider) {
        let Self { log, claude, local, .. } = self;
        let slot = match provider {
            Provider::Claude => claude,
            Provider::Local => local,
        };
        let Some(backend) = slot.backend.as_mut() else { return };
        for rec in &log[slot.synced..] {
            if rec.provider != provider && !(rec.private && provider == Provider::Claude) {
                backend.push_user(&rec.user);
                backend.push_assistant_text(&rec.render());
            }
        }
        slot.synced = log.len();
    }

    fn run_on(&mut self, provider: Provider, text: &str, private: bool, ui: &mut dyn Ui) -> (TurnRecord, Result<()>) {
        self.sync(provider);
        let mut record =
            TurnRecord { user: text.to_string(), provider, private, reply: String::new(), tools: Vec::new() };
        let result = self.tool_loop(provider, &mut record, ui);
        (record, result)
    }

    fn tool_loop(&mut self, provider: Provider, record: &mut TurnRecord, ui: &mut dyn Ui) -> Result<()> {
        let Self { orion, router, claude, local, max_tool_rounds, .. } = self;
        let backend = match provider {
            Provider::Claude => claude.backend.as_mut(),
            Provider::Local => local.backend.as_mut(),
        }
        .expect("checked by turn");
        backend.push_user(&record.user);
        for _ in 0..*max_tool_rounds {
            let reply = backend.respond(&mut |ev| match ev {
                Event::Text(t) => {
                    record.reply.push_str(t);
                    ui.text(t);
                }
                Event::Thinking(t) => ui.thinking(t),
            })?;
            match reply.stop {
                Stop::Refused => {
                    ui.notice("The model declined this request.");
                    return Ok(());
                }
                _ if reply.calls.is_empty() => {
                    if reply.stop == Stop::Truncated {
                        ui.notice("The reply hit the output limit and was cut off.");
                    }
                    return Ok(());
                }
                _ => {}
            }
            let results = reply
                .calls
                .iter()
                .map(|call| {
                    let (content, is_error) = if let Some(raw) = &call.invalid_input {
                        (serde_json::json!({ "INVALID_JSON": raw }).to_string(), true)
                    } else if reply.stop == Stop::Truncated {
                        ("Not run: the tool input was cut off by the output limit.".to_string(), true)
                    } else {
                        run_tool(orion, router, provider, call, ui)
                    };
                    record.tools.push(summarize(call, &content, is_error));
                    ToolResult { id: call.id.clone(), content, is_error }
                })
                .collect();
            backend.push_tool_results(results);
        }
        ui.notice(&format!("Stopped after {max_tool_rounds} model calls in one turn."));
        Ok(())
    }
}

fn run_tool(orion: &Orion, router: &Router, provider: Provider, call: &ToolCall, ui: &mut dyn Ui) -> (String, bool) {
    ui.tool_start(&call.name, &call.input);
    let start = Instant::now();
    let result = match router.private_path_in(&call.input) {
        // Private files never reach Claude, whatever the policy allows.
        Some(path) if provider == Provider::Claude => {
            Err(format!("{path} is private: only the local model may access it (chat.router.private_paths)"))
        }
        _ => {
            let approver = UiApprover(RefCell::new(&mut *ui));
            orion.call(&call.name, call.input.clone(), Some(&approver)).map_err(|e| e.to_string())
        }
    };
    ui.tool_end(&call.name, &result, start.elapsed());
    match result {
        Ok(v) => (serde_json::to_string_pretty(&v).unwrap_or_default(), false),
        Err(e) => (e, true),
    }
}

/// One line per tool call for the other model: what ran and a taste of
/// what came back.
fn summarize(call: &ToolCall, content: &str, is_error: bool) -> String {
    let squash = |s: &str, max: usize| {
        let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
        if flat.len() <= max {
            return flat;
        }
        let mut end = max;
        while !flat.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &flat[..end])
    };
    let outcome = if is_error { "error" } else { "ok" };
    format!("{} {} → {outcome}: {}", call.name, squash(&call.input.to_string(), 200), squash(content, 300))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::rc::Rc;

    use serde_json::json;

    use super::super::test_support::{orion, Recorder};
    use super::super::{Reply, Usage};
    use super::*;

    type Log = Rc<RefCell<Vec<String>>>;

    /// Replies from a script and logs every push.
    struct Scripted {
        name: &'static str,
        script: VecDeque<Result<(String, Vec<ToolCall>)>>,
        log: Log,
    }

    impl Scripted {
        fn new(name: &'static str, log: &Log, script: Vec<Result<(String, Vec<ToolCall>)>>) -> Box<Self> {
            Box::new(Self { name, script: script.into(), log: log.clone() })
        }
    }

    impl Backend for Scripted {
        fn describe(&self) -> String {
            self.name.to_string()
        }
        fn model(&self) -> String {
            self.name.to_string()
        }
        fn set_model(&mut self, _model: &str) {}
        fn push_user(&mut self, text: &str) {
            self.log.borrow_mut().push(format!("{} user: {text}", self.name));
        }
        fn push_assistant_text(&mut self, text: &str) {
            self.log.borrow_mut().push(format!("{} assistant: {text}", self.name));
        }
        fn respond(&mut self, on: &mut dyn FnMut(Event)) -> Result<Reply> {
            let (text, calls) = self.script.pop_front().unwrap_or_else(|| Err(anyhow::anyhow!("script ran out")))?;
            on(Event::Text(&text));
            let stop = if calls.is_empty() { Stop::Done } else { Stop::ToolUse };
            Ok(Reply { calls, stop })
        }
        fn push_tool_results(&mut self, results: Vec<ToolResult>) {
            for r in results {
                self.log.borrow_mut().push(format!("{} result: {}", self.name, if r.is_error { &r.content } else { "ok" }));
            }
        }
        fn clear(&mut self) {}
        fn usage(&self) -> Usage {
            Usage::default()
        }
    }

    fn say(text: &str) -> Result<(String, Vec<ToolCall>)> {
        Ok((text.to_string(), Vec::new()))
    }

    fn entries(log: &Log, prefix: &str) -> Vec<String> {
        log.borrow().iter().filter(|l| l.starts_with(prefix)).cloned().collect()
    }

    #[test]
    fn routes_each_message_and_shares_the_transcript() {
        let log = Log::default();
        let orion = orion();
        let claude = Scripted::new("claude", &log, vec![say("Here is the plan."), say("Cleaning up.")]);
        let local = Scripted::new("local", &log, vec![say("Disk is 28% full.")]);
        let mut s = Session::with_backends(&orion, Mode::Auto, Some(claude), Some(local));
        let mut ui = Recorder::default();

        s.turn("how full is my disk?", &mut ui).unwrap();
        s.turn("plan a cleanup of my home directory", &mut ui).unwrap();
        s.turn("ok, go ahead", &mut ui).unwrap();
        assert!(ui.out.contains("→the local model(short request)"), "{}", ui.out);
        assert!(ui.out.contains("→Claude(complex: \"plan\")"), "{}", ui.out);
        assert!(ui.out.ends_with("→Claude(follow-up)Cleaning up."), "{}", ui.out);

        // Claude was caught up on the local turn before answering.
        let c = entries(&log, "claude");
        assert_eq!(c[0], "claude user: how full is my disk?");
        assert!(c[1].starts_with("claude assistant: [The local model handled this message.]\n\nDisk is 28% full."));
        assert_eq!(c[2], "claude user: plan a cleanup of my home directory");
    }

    struct Judge(Provider);

    impl Classifier for Judge {
        fn name(&self) -> String {
            "judge".into()
        }
        fn classify(&self, _text: &str, _previous: Option<(Provider, &str)>) -> Result<router::Verdict> {
            Ok(router::Verdict { provider: self.0, reason: "said so".into() })
        }
    }

    #[test]
    fn the_classifier_decides_complexity() {
        let log = Log::default();
        let orion = orion();
        let claude = Scripted::new("claude", &log, vec![say("Sure.")]);
        let local = Scripted::new("local", &log, vec![]);
        let mut s = Session::with_backends(&orion, Mode::Auto, Some(claude), Some(local));
        s.set_classifier(Some(Box::new(Judge(Provider::Claude))));
        assert_eq!(s.classifier_name().as_deref(), Some("judge"));
        let mut ui = Recorder::default();
        // Rules would keep this local ("short request"); the classifier says Claude.
        s.turn("how full is my disk?", &mut ui).unwrap();
        assert!(ui.out.starts_with("→Claude(judge: said so ("), "{}", ui.out);
    }

    #[test]
    fn private_turns_never_reach_claude() {
        let log = Log::default();
        let orion = orion();
        let claude = Scripted::new("claude", &log, vec![say("Refactored.")]);
        let local = Scripted::new("local", &log, vec![say("Stored it."), say("Fine.")]);
        let mut s = Session::with_backends(&orion, Mode::Auto, Some(claude), Some(local));
        let mut ui = Recorder::default();

        s.turn("remember my bank password is hunter2", &mut ui).unwrap();
        s.turn("refactor the backup script", &mut ui).unwrap();
        assert!(log.borrow().iter().filter(|l| l.starts_with("claude")).all(|l| !l.contains("hunter2")));

        // The local model hears about Claude's turn, though.
        s.turn("@local all good?", &mut ui).unwrap();
        let l = entries(&log, "local");
        assert!(l.iter().any(|e| e.contains("Claude handled this message") && e.contains("Refactored.")), "{l:?}");
    }

    #[test]
    fn private_without_local_asks_first() {
        let log = Log::default();
        let orion = orion();
        let mut s = Session::with_backends(&orion, Mode::Auto, Some(Scripted::new("claude", &log, vec![])), None);
        let mut ui = Recorder::default(); // declines confirmations
        s.turn("my password is hunter2", &mut ui).unwrap();
        assert!(ui.out.contains("!Not sent."));
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn falls_back_when_the_routed_model_fails() {
        let log = Log::default();
        let orion = orion();
        let claude = Scripted::new("claude", &log, vec![Err(anyhow::anyhow!("529 overloaded"))]);
        let local = Scripted::new("local", &log, vec![say("Here's a rough design.")]);
        let mut s = Session::with_backends(&orion, Mode::Auto, Some(claude), Some(local));
        let mut ui = Recorder::default();
        s.turn("design a backup scheme", &mut ui).unwrap();
        assert!(ui.out.contains("Claude failed (529 overloaded); trying the local model."), "{}", ui.out);
        assert!(ui.out.ends_with("Here's a rough design."));
    }

    #[test]
    fn claude_cannot_touch_private_paths() {
        let log = Log::default();
        let orion = orion();
        let read = |id: &str| ToolCall {
            id: id.into(),
            name: "fs_read".into(),
            input: json!({ "path": "~/.ssh/id_ed25519" }),
            invalid_input: None,
        };
        let claude = Scripted::new("claude", &log, vec![Ok((String::new(), vec![read("c")])), say("Blocked.")]);
        let local = Scripted::new("local", &log, vec![Ok((String::new(), vec![read("l")])), say("Read it.")]);
        let mut s = Session::with_backends(&orion, Mode::Claude, Some(claude), Some(local));
        let mut ui = Recorder::default();
        s.turn("@claude show the key file", &mut ui).unwrap();
        s.turn("@local show the key file", &mut ui).unwrap();
        let results: Vec<String> = log.borrow().iter().filter(|l| l.contains(" result: ")).cloned().collect();
        assert!(results[0].starts_with("claude result: ~/.ssh is private"), "{results:?}");
        // The local model gets past the guard (and then fails normally: no such file here).
        assert!(!results[1].contains("is private"), "{results:?}");
    }

}
