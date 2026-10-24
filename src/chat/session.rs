//! One conversation across up to two models. Every user message is routed
//! to Claude or the local model (or pinned to one), and both share a single
//! transcript: before a model answers, it is caught up on the turns the
//! other one handled. Private turns are never shown to Claude.

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use super::classifier::LocalClassifier;
use super::router::{self, Classifier, Route, RouteContext, Router, Strategy};
use super::{connect, Backend, Event, Mode, Provider, Stop, ToolCall, ToolResult, Ui, UiApprover};
use super::journal::{self, Journal};
use super::memory::{self, Memory};
use super::store::{self, Conversation, SavedBackend, Turn};
use crate::{interrupt, Orion};

/// A finished turn of the shared transcript.
type TurnRecord = Turn;

impl Turn {
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
    /// Saved history for a backend that was not connected when the
    /// conversation was resumed; loaded when it connects, and kept in the
    /// file until then.
    stored: Option<SavedBackend>,
}

impl Slot {
    fn attach(&mut self, mut backend: Box<dyn Backend>) {
        if let Some(saved) = self.stored.take() {
            match backend.import_history(saved.history.clone()) {
                Ok(()) => self.synced = saved.synced,
                Err(e) => {
                    tracing::warn!("could not restore saved history: {e:#}");
                    self.stored = Some(saved);
                }
            }
        }
        self.backend = Some(backend);
        self.error = None;
    }
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
    id: String,
    /// Set by `/title` or a resumed conversation; otherwise derived from the
    /// first message.
    title: Option<String>,
    created_ms: u64,
    /// Whose conversation this is: someone enrolled by voice, or the
    /// person at the keyboard (the login name).
    person: String,
    /// What is remembered about them (when `chat.memory` is on).
    memory: Option<Memory>,
    memory_dir: Option<PathBuf>,
    /// Their journal (when `chat.journal.enabled` is on).
    journal: Option<Journal>,
    journal_home: Option<PathBuf>,
    /// Whether the local backend runs on this machine, so that what is
    /// private may go to it. A hosted API behind it (Gemini) does not.
    local_on_machine: bool,
    /// The memory part of the system prompt, fixed for a conversation so a
    /// note saved mid-way doesn't upset prompt caching: (Claude, local).
    context: (String, String),
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
        s.refresh_context();
        Ok(s)
    }

    fn build_classifier(&self) -> Option<Box<dyn Classifier>> {
        let chat = &self.orion.policy().chat;
        // It sees every message that isn't private, so it must be here.
        if chat.router.strategy != Strategy::Model || !self.local_on_machine {
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
            id: store::new_id(),
            title: None,
            created_ms: store::now_ms(),
            person: owner_name(),
            memory: None,
            memory_dir: chat.memory.then(memory::default_dir).flatten(),
            journal: None,
            journal_home: chat.journal.enabled.then(|| chat.journal.home()).flatten(),
            local_on_machine: chat.local.on_machine(),
            context: Default::default(),
        }
    }

    /// Whose conversation this is.
    pub fn person(&self) -> &str {
        &self.person
    }

    pub fn memory(&self) -> Option<&Memory> {
        self.memory.as_ref()
    }

    pub fn memory_mut(&mut self) -> Option<&mut Memory> {
        self.memory.as_mut()
    }

    /// Keep memories somewhere else (tests), or nowhere (`None`).
    pub fn set_memory_dir(&mut self, dir: Option<PathBuf>) {
        self.memory_dir = dir;
        self.memory = None;
        self.refresh_context();
    }

    pub fn journal(&self) -> Option<&Journal> {
        self.journal.as_ref()
    }

    /// Keep the journal somewhere else (tests), or nowhere (`None`).
    pub fn set_journal_home(&mut self, home: Option<PathBuf>) {
        self.journal_home = home;
        self.journal = None;
        self.refresh_context();
    }

    /// Whether `p` may be given what is private: only a model on this
    /// machine.
    fn keeps_private(&self, p: Provider) -> bool {
        p == Provider::Local && self.local_on_machine
    }

    /// Make this `person`'s conversation: their memory, in the prompt.
    pub fn set_person(&mut self, person: &str) {
        self.person = person.to_string();
        self.memory = None;
        self.refresh_context();
    }

    /// Load the person's memory and put it in every model's system prompt.
    fn refresh_context(&mut self) {
        if self.memory.as_ref().is_none_or(|m| m.person != self.person) {
            self.memory = self.memory_dir.as_ref().and_then(|d| match Memory::load(d, &self.person) {
                Ok(m) => Some(m),
                Err(e) => {
                    tracing::warn!("memory: {e:#}");
                    None
                }
            });
        }
        self.journal = self.journal_home.as_ref().map(|h| Journal::open(h, &self.person));
        let name = crate::voice::capitalize(&self.person);
        let (mut for_claude, mut for_local) = match &self.memory {
            Some(m) => (m.prompt(&name, false), m.prompt(&name, self.local_on_machine)),
            None => Default::default(),
        };
        if let Some(j) = &self.journal {
            for text in [&mut for_claude, &mut for_local] {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&j.prompt(&name));
            }
        }
        self.context = (for_claude, for_local);
        for p in [Provider::Claude, Provider::Local] {
            let system = self.system_for(p);
            if let Some(b) = self.slot_mut(p).backend.as_mut() {
                b.set_system(&system);
            }
        }
    }

    fn system_for(&self, p: Provider) -> String {
        let base = super::system_prompt(self.orion.policy());
        let extra = match p {
            Provider::Claude => &self.context.0,
            Provider::Local => &self.context.1,
        };
        if extra.is_empty() { base } else { format!("{base}\n\n{extra}") }
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
                let connected = connect(self.orion, p, None).map(|mut b| {
                    b.set_system(&self.system_for(p));
                    b
                });
                let slot = self.slot_mut(p);
                match connected {
                    Ok(b) => slot.attach(b),
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

    /// Start a new conversation (the old one stays saved).
    pub fn clear(&mut self) {
        for p in [Provider::Claude, Provider::Local] {
            let slot = self.slot_mut(p);
            if let Some(b) = slot.backend.as_mut() {
                b.clear();
            }
            slot.synced = 0;
            slot.stored = None;
        }
        self.log.clear();
        self.previous = None;
        self.id = store::new_id();
        self.title = None;
        self.created_ms = store::now_ms();
        self.refresh_context();
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn title(&self) -> String {
        self.title.clone().or_else(|| self.log.first().map(store::title_from)).unwrap_or_default()
    }

    pub fn set_title(&mut self, title: &str) {
        self.title = Some(title.trim().to_string());
    }

    pub fn turns(&self) -> &[Turn] {
        &self.log
    }

    /// Everything needed to resume this conversation; `None` while it is
    /// still empty.
    pub fn snapshot(&self) -> Option<Conversation> {
        if self.log.is_empty() {
            return None;
        }
        let mut backends = std::collections::BTreeMap::new();
        for (p, key) in [(Provider::Claude, "claude"), (Provider::Local, "local")] {
            let slot = self.slot(p);
            let saved = match &slot.backend {
                Some(b) => Some(SavedBackend { model: b.model(), synced: slot.synced, history: b.export_history() }),
                None => slot.stored.clone(),
            };
            if let Some(saved) = saved {
                backends.insert(key.to_string(), saved);
            }
        }
        Some(Conversation {
            version: store::FORMAT_VERSION,
            id: self.id.clone(),
            title: self.title(),
            created_ms: self.created_ms,
            updated_ms: store::now_ms(),
            mode: self.mode,
            previous: self.previous,
            turns: self.log.clone(),
            backends,
            owner: self.person.clone(),
        })
    }

    /// Continue a saved conversation. Each connected model gets its own
    /// history back exactly as saved (and its model); a model that is not
    /// connected gets it when it connects. The mode is left to the caller.
    pub fn restore(&mut self, conv: Conversation) {
        self.clear();
        self.id = conv.id;
        self.title = Some(conv.title);
        self.created_ms = conv.created_ms;
        self.previous = conv.previous;
        self.log = conv.turns;
        self.person = if conv.owner.is_empty() { owner_name() } else { conv.owner };
        self.memory = None;
        self.refresh_context();
        for (p, key) in [(Provider::Claude, "claude"), (Provider::Local, "local")] {
            let Some(saved) = conv.backends.get(key).cloned() else { continue };
            let slot = self.slot_mut(p);
            slot.stored = Some(saved.clone());
            if let Some(b) = slot.backend.take() {
                let mut b = b;
                if !saved.model.is_empty() {
                    b.set_model(&saved.model);
                }
                slot.attach(b);
            }
        }
    }

    /// What the router would do with `text`, without sending it.
    pub fn explain_route(&self, text: &str) -> String {
        match self.decide(text) {
            Route::To { provider, reason } => format!("{} ({reason})", router::name(provider)),
            Route::PrivateButNoLocal { reason } => format!("would ask you first ({reason}; no model on this machine)"),
        }
    }

    fn decide(&self, text: &str) -> Route {
        let ctx = RouteContext {
            previous: self.previous,
            previous_text: self.log.last().map(|r| r.user.as_str()),
            claude_available: self.claude.backend.is_some(),
            local_available: self.local.backend.is_some(),
        };
        match self.router.route(text, &ctx, self.classifier.as_deref()) {
            // The local backend is a hosted API: nothing here can keep it.
            Route::To { provider: Provider::Local, reason } if !self.local_on_machine && reason.starts_with("private") => {
                Route::PrivateButNoLocal { reason }
            }
            route => route,
        }
    }

    /// Send one user message and run tools until the model is done. A
    /// leading `@claude` or `@local` sends it to that model regardless of
    /// the mode.
    pub fn turn(&mut self, input: &str, ui: &mut dyn Ui) -> Result<()> {
        interrupt::clear();
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
                    // The configured local model, even if it's hosted, else Claude.
                    let (to, name) = match self.local.backend.as_ref() {
                        Some(b) => (Provider::Local, b.model()),
                        None => (Provider::Claude, "Claude".to_string()),
                    };
                    if !ui.confirm(&format!("This looks private ({reason}) and no model on this machine is available. Send it to {name}?")) {
                        ui.notice("Not sent.");
                        return Ok(());
                    }
                    (to, "you confirmed".to_string(), false)
                }
            },
        };
        if interrupt::requested() {
            ui.notice("Interrupted.");
            return Ok(());
        }
        if self.slot(provider).backend.is_none() {
            let why = self.slot(provider).error.clone().unwrap_or_else(|| "not connected".into());
            bail!("{} is not available: {why}", router::name(provider));
        }
        if self.mode == Mode::Auto || forced.is_some() {
            ui.route(router::name(provider), &self.describe(provider), &reason);
        }
        // Private only while it stays on this machine; sending it elsewhere
        // anyway (on purpose) makes it ordinary history.
        let private = private_reason.is_some() && self.keeps_private(provider);

        let (record, result) = self.run_on(provider, text, private, ui);
        let started = !record.reply.is_empty() || !record.tools.is_empty();
        self.finish(record);
        let Err(e) = result else { return Ok(()) };
        if interrupt::is_interrupt(&e) {
            ui.notice("Interrupted.");
            return Ok(());
        }

        // In auto mode, a model that fails before saying anything hands the
        // message to the other one, but private messages never go to Claude.
        let other = router::other(provider);
        if !routed || started || self.slot(other).backend.is_none() || (private_reason.is_some() && !self.keeps_private(other)) {
            return Err(e);
        }
        ui.notice(&format!("{} failed ({e:#}); trying {}.", capitalized(router::name(provider)), router::name(other)));
        ui.route(router::name(other), &self.describe(other), "fallback");
        let (record, result) = self.run_on(other, text, private_reason.is_some() && self.keeps_private(other), ui);
        self.finish(record);
        match result {
            Err(e) if interrupt::is_interrupt(&e) => {
                ui.notice("Interrupted.");
                Ok(())
            }
            other => other,
        }
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
        let keeps_private = self.keeps_private(provider);
        let Self { log, claude, local, .. } = self;
        let slot = match provider {
            Provider::Claude => claude,
            Provider::Local => local,
        };
        let Some(backend) = slot.backend.as_mut() else { return };
        for rec in &log[slot.synced..] {
            if rec.provider != provider && !(rec.private && !keeps_private) {
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
        // What the journal holds on this, added to what the model is sent
        // (the transcript keeps the message as written).
        let sent = match self.recall(provider, text) {
            Some(note) => {
                ui.thinking(&format!("recalled from the journal: {}", note.lines().count() - 1));
                format!("{text}\n\n{note}")
            }
            None => text.to_string(),
        };
        let result = self.tool_loop(provider, &sent, &mut record, ui);
        (record, result)
    }

    /// The journal entries that match `text`, as a note for `provider`.
    fn recall(&self, provider: Provider, text: &str) -> Option<String> {
        let config = &self.orion.policy().chat.journal;
        let journal = self.journal.as_ref().filter(|_| config.auto_recall)?;
        let hits = journal.recall(text, config.recall_limit, self.keeps_private(provider));
        (!hits.is_empty()).then(|| journal::recall_note(&hits))
    }

    fn tool_loop(&mut self, provider: Provider, sent: &str, record: &mut TurnRecord, ui: &mut dyn Ui) -> Result<()> {
        let keeps_private = self.keeps_private(provider);
        let Self { orion, router, claude, local, max_tool_rounds, memory, journal, .. } = self;
        let backend = match provider {
            Provider::Claude => claude.backend.as_mut(),
            Provider::Local => local.backend.as_mut(),
        }
        .expect("checked by turn");
        backend.push_user(sent);
        for _ in 0..*max_tool_rounds {
            let reply = backend.respond(&mut |ev| match ev {
                Event::Text(t) => {
                    record.reply.push_str(t);
                    ui.text(t);
                }
                Event::Thinking(t) => ui.thinking(t),
                Event::Notice(t) => ui.notice(t),
            });
            let reply = match reply {
                Err(e) if interrupt::is_interrupt(&e) => {
                    // Keep what was said, so the model knows where it stopped.
                    if !record.reply.trim().is_empty() {
                        record.reply = format!("{} [interrupted by the user]", record.reply.trim_end());
                        backend.push_assistant_text(&record.reply);
                    }
                    return Err(e);
                }
                other => other?,
            };
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
                    let (content, is_error) = if interrupt::requested() {
                        ("Not run: interrupted by the user.".to_string(), true)
                    } else if let Some(raw) = &call.invalid_input {
                        (serde_json::json!({ "INVALID_JSON": raw }).to_string(), true)
                    } else if reply.stop == Stop::Truncated {
                        ("Not run: the tool input was cut off by the output limit.".to_string(), true)
                    } else {
                        let mine = Mine { memory: memory.as_mut(), journal: journal.as_ref(), keeps_private };
                        run_tool(orion, router, call, mine, ui)
                    };
                    let input = orion.tools().find(|(t, _)| t.name() == call.name).map_or_else(|| call.input.clone(), |(t, _)| t.redact(&call.input));
                    record.tools.push(summarize(&call.name, &input, &content, is_error));
                    ToolResult { id: call.id.clone(), content, is_error }
                })
                .collect();
            backend.push_tool_results(results);
            interrupt::check()?;
        }
        ui.notice(&format!("Stopped after {max_tool_rounds} model calls in one turn."));
        Ok(())
    }
}

/// What a tool call may touch of Orion's own: the person's memory and
/// journal, and whether the model may see what is private.
struct Mine<'m> {
    memory: Option<&'m mut Memory>,
    journal: Option<&'m Journal>,
    keeps_private: bool,
}

fn run_tool(orion: &Orion, router: &Router, call: &ToolCall, mine: Mine, ui: &mut dyn Ui) -> (String, bool) {
    let Mine { memory, journal, keeps_private } = mine;
    let shown = orion.tools().find(|(t, _)| t.name() == call.name).map_or_else(|| call.input.clone(), |(t, _)| t.redact(&call.input));
    ui.tool_start(&call.name, &shown);
    let start = Instant::now();
    if memory::is_memory_tool(&call.name) || journal::is_journal_tool(&call.name) {
        // Orion's own memory of the person, not the machine: no policy, but
        // it goes in the audit log like everything else.
        let private_reason = |t: &str| router.private_reason(t);
        let result = match (memory, journal) {
            (Some(m), _) if memory::is_memory_tool(&call.name) => memory::call(m, &call.name, &call.input, private_reason),
            (_, Some(j)) if journal::is_journal_tool(&call.name) => journal::call(j, &call.name, &call.input, private_reason, keeps_private),
            _ if memory::is_memory_tool(&call.name) => Err(anyhow::anyhow!("memory is off (chat.memory)")),
            _ => Err(anyhow::anyhow!("the journal is off (chat.journal.enabled)")),
        }
        .map_err(|e| format!("{e:#}"));
        orion.record(&call.name, &call.input, ui.caller().as_deref(), result.as_ref().err().map(String::as_str));
        ui.tool_end(&call.name, &result, start.elapsed());
        return match result {
            Ok(v) => (v.to_string(), false),
            Err(e) => (e, true),
        };
    }
    let risk = orion.tools().find(|(t, _)| t.name() == call.name).map(|(t, _)| t.risk());
    let caller = ui.caller();
    let refused = risk.and_then(|r| ui.refuses(&call.name, r));
    let result = match (router.private_path_in(&call.input), refused) {
        (_, Some(reason)) => Err(orion.refuse(&call.name, &shown, &reason, caller.as_deref()).to_string()),
        // Private files never leave this machine, whatever the policy allows.
        (Some(path), None) if !keeps_private => {
            Err(format!("{path} is private: only a model on this machine may access it (chat.router.private_paths)"))
        }
        _ => {
            let approver = UiApprover(RefCell::new(&mut *ui));
            orion.call_as(&call.name, call.input.clone(), Some(&approver), caller.as_deref()).map_err(|e| e.to_string())
        }
    };
    ui.tool_end(&call.name, &result, start.elapsed());
    match result {
        Ok(v) => (serde_json::to_string_pretty(&v).unwrap_or_default(), false),
        Err(e) => (e, true),
    }
}

/// Whose conversations typed messages belong to: the login name.
pub fn owner_name() -> String {
    crate::voice::speaker::default_name()
}

/// One line per tool call for the other model: what ran and a taste of
/// what came back.
fn summarize(name: &str, input: &serde_json::Value, content: &str, is_error: bool) -> String {
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
    format!("{name} {} → {outcome}: {}", squash(&input.to_string(), 200), squash(content, 300))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use serde_json::{json, Value};

    use super::super::test_support::{orion, Recorder};
    use super::super::{Reply, Usage};
    use super::*;

    type Log = Arc<Mutex<Vec<String>>>;

    /// Replies from a script and logs every push.
    struct Scripted {
        name: &'static str,
        script: VecDeque<Result<(String, Vec<ToolCall>)>>,
        log: Log,
        /// What it has been told, as its "history".
        history: Vec<String>,
        model: String,
        system: String,
    }

    impl Scripted {
        fn new(name: &'static str, log: &Log, script: Vec<Result<(String, Vec<ToolCall>)>>) -> Box<Self> {
            Box::new(Self { name, script: script.into(), log: log.clone(), history: Vec::new(), model: name.into(), system: String::new() })
        }

        fn note(&mut self, entry: String) {
            self.history.push(entry.clone());
            self.log.lock().unwrap().push(entry);
        }
    }

    impl Backend for Scripted {
        fn describe(&self) -> String {
            self.name.to_string()
        }
        fn model(&self) -> String {
            self.model.clone()
        }
        fn set_model(&mut self, model: &str) {
            self.model = model.into();
        }
        fn set_system(&mut self, system: &str) {
            self.system = system.to_string();
        }
        fn push_user(&mut self, text: &str) {
            self.note(format!("{} user: {text}", self.name));
        }
        fn push_assistant_text(&mut self, text: &str) {
            self.note(format!("{} assistant: {text}", self.name));
        }
        fn respond(&mut self, on: &mut dyn FnMut(Event)) -> Result<Reply> {
            let (text, calls) = self.script.pop_front().unwrap_or_else(|| Err(anyhow::anyhow!("script ran out")))?;
            on(Event::Text(&text));
            self.history.push(format!("{} said: {text}", self.name));
            let stop = if calls.is_empty() { Stop::Done } else { Stop::ToolUse };
            Ok(Reply { calls, stop })
        }
        fn push_tool_results(&mut self, results: Vec<ToolResult>) {
            for r in results {
                self.log.lock().unwrap().push(format!("{} result: {}", self.name, if r.is_error { &r.content } else { "ok" }));
            }
        }
        fn clear(&mut self) {
            self.history.clear();
        }
        fn export_history(&self) -> Value {
            json!(self.history)
        }
        fn import_history(&mut self, history: Value) -> Result<()> {
            self.history = serde_json::from_value(history)?;
            Ok(())
        }
        fn usage(&self) -> Usage {
            Usage::default()
        }
    }

    fn say(text: &str) -> Result<(String, Vec<ToolCall>)> {
        Ok((text.to_string(), Vec::new()))
    }

    fn entries(log: &Log, prefix: &str) -> Vec<String> {
        log.lock().unwrap().iter().filter(|l| l.starts_with(prefix)).cloned().collect()
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
        assert!(log.lock().unwrap().iter().filter(|l| l.starts_with("claude")).all(|l| !l.contains("hunter2")));

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
        assert!(log.lock().unwrap().is_empty());
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

    /// Presses Ctrl-C when the first tool starts.
    #[derive(Default)]
    struct Impatient {
        inner: Recorder,
        flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    }

    impl Ui for Impatient {
        fn text(&mut self, t: &str) {
            self.inner.text(t)
        }
        fn thinking(&mut self, t: &str) {
            self.inner.thinking(t)
        }
        fn tool_start(&mut self, name: &str, input: &Value) {
            if let Some(f) = &self.flag {
                f.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            self.inner.tool_start(name, input)
        }
        fn tool_end(&mut self, name: &str, r: &Result<Value, String>, took: Duration) {
            self.inner.tool_end(name, r, took)
        }
        fn notice(&mut self, t: &str) {
            self.inner.notice(t)
        }
        fn approve(&mut self, tool: &dyn crate::Tool, args: &Value) -> bool {
            self.inner.approve(tool, args)
        }
    }

    #[test]
    fn ctrl_c_between_tools_answers_every_call_and_stops() {
        let log = Log::default();
        let orion = orion();
        let info = |id: &str| ToolCall { id: id.into(), name: "system_info".into(), input: json!({}), invalid_input: None };
        let local = Scripted::new("local", &log, vec![Ok(("Checking.".into(), vec![info("a"), info("b")])), say("never")]);
        let mut s = Session::with_backends(&orion, Mode::Local, None, Some(local));
        let mut ui = Impatient { flag: Some(interrupt::test_flag::install()), ..Default::default() };
        s.turn("look around", &mut ui).unwrap();
        assert!(ui.inner.out.ends_with("!Interrupted."), "{}", ui.inner.out);
        let results = entries(&log, "local result");
        assert_eq!(results, ["local result: ok", "local result: Not run: interrupted by the user."]);
        // The next turn starts clean.
        let mut next = Recorder::default();
        s.turn("@local hi", &mut next).unwrap();
        assert!(next.out.ends_with("never"), "{}", next.out);
    }

    /// Answers like `Recorder`, but turns away tools that change things,
    /// as for someone with `tools = "read"`.
    #[derive(Default)]
    struct ReadOnly {
        inner: Recorder,
    }

    impl Ui for ReadOnly {
        fn text(&mut self, t: &str) {
            self.inner.text(t)
        }
        fn thinking(&mut self, t: &str) {
            self.inner.thinking(t)
        }
        fn tool_start(&mut self, name: &str, input: &Value) {
            self.inner.tool_start(name, input)
        }
        fn tool_end(&mut self, name: &str, r: &Result<Value, String>, took: Duration) {
            self.inner.tool_end(name, r, took)
        }
        fn notice(&mut self, t: &str) {
            self.inner.notice(t)
        }
        fn approve(&mut self, tool: &dyn crate::Tool, args: &Value) -> bool {
            panic!("asked to approve {}: a refused tool must not get that far ({args})", tool.name())
        }
        fn refuses(&mut self, _tool: &str, risk: crate::tool::Risk) -> Option<String> {
            crate::voice::ToolAccess::Read.refuses("ana", risk)
        }
    }

    #[test]
    fn a_turn_can_refuse_tools_before_anyone_is_asked() {
        let log = Log::default();
        let orion = orion();
        let call = |id: &str, name: &str, input: Value| ToolCall { id: id.into(), name: name.into(), input, invalid_input: None };
        let calls = vec![call("a", "system_info", json!({})), call("b", "fs_write", json!({ "path": "/tmp/x", "content": "y" }))];
        let local = Scripted::new("local", &log, vec![Ok(("Checking.".into(), calls)), say("done")]);
        let mut s = Session::with_backends(&orion, Mode::Local, None, Some(local));
        s.turn("Ana (by voice): write a note", &mut ReadOnly::default()).unwrap();
        let results = entries(&log, "local result");
        assert_eq!(results[0], "local result: ok");
        assert!(results[1].contains("ana may only use tools that look at the machine"), "{results:?}");
    }

    #[test]
    fn remembers_each_person_and_keeps_private_notes_local() {
        let log = Log::default();
        let orion = orion();
        let dir = tempfile::tempdir().unwrap();
        let save = |id: &str, text: &str| ToolCall {
            id: id.into(),
            name: "memory_save".into(),
            input: json!({ "text": text }),
            invalid_input: None,
        };
        let calls = vec![save("a", "Prefers metric units"), save("b", "Has a medical appointment on Tuesdays")];
        let local = Scripted::new("local", &log, vec![Ok(("Noted.".into(), calls)), say("done")]);
        let claude = Scripted::new("claude", &log, vec![]);
        let mut s = Session::with_backends(&orion, Mode::Local, Some(claude), Some(local));
        s.set_memory_dir(Some(dir.path().into()));
        s.set_person("ana");
        s.turn("Ana (by voice): remember these", &mut Recorder::default()).unwrap();
        assert_eq!(entries(&log, "local result"), ["local result: ok", "local result: ok"]);
        let saved = Memory::load(dir.path(), "ana").unwrap();
        assert_eq!(saved.notes.len(), 2);
        assert!(saved.notes[1].private, "it mentions \"medical\"");

        // Her next conversation starts with what was remembered; Claude
        // never gets the private note.
        s.clear();
        let (for_claude, for_local) = (s.system_for(Provider::Claude), s.system_for(Provider::Local));
        assert!(for_claude.contains("You are talking with Ana.") && for_claude.contains("[1] Prefers metric units"));
        assert!(!for_claude.contains("medical"), "{for_claude}");
        assert!(for_local.contains("[2] Has a medical appointment on Tuesdays"));
        // Ben's are his own.
        s.set_person("ben");
        assert!(s.system_for(Provider::Local).contains("You are talking with Ben. You don't remember anything"));
        assert!(!s.system_for(Provider::Local).contains("metric"));
    }

    #[test]
    fn ctrl_c_mid_reply_keeps_the_partial_and_skips_fallback() {
        let log = Log::default();
        let orion = orion();
        let claude = Scripted::new("claude", &log, vec![Err(interrupt::Interrupted.into())]);
        let local = Scripted::new("local", &log, vec![]);
        let mut s = Session::with_backends(&orion, Mode::Auto, Some(claude), Some(local));
        let mut ui = Recorder::default();
        s.turn("design a backup scheme", &mut ui).unwrap();
        assert!(ui.out.ends_with("!Interrupted."), "{}", ui.out);
        assert!(!ui.out.contains("trying"), "an interrupt is not a failure: {}", ui.out);
        assert!(entries(&log, "local").is_empty());
    }

    #[test]
    fn snapshot_and_restore_continue_the_conversation() {
        let log = Log::default();
        let orion = orion();
        let claude = Scripted::new("claude", &log, vec![say("Here's the plan.")]);
        let local = Scripted::new("local", &log, vec![say("28% full.")]);
        let mut s = Session::with_backends(&orion, Mode::Auto, Some(claude), Some(local));
        assert!(s.snapshot().is_none(), "nothing to save yet");
        let mut ui = Recorder::default();
        s.turn("how full is my disk?", &mut ui).unwrap();
        s.turn("plan a cleanup of my home", &mut ui).unwrap();
        s.backend_mut(Provider::Claude).unwrap().set_model("claude-sonnet-5-5");
        let saved = s.snapshot().unwrap();
        assert_eq!(saved.title, "how full is my disk?");
        assert_eq!(saved.turns.len(), 2);
        assert_eq!(saved.backends["claude"].synced, 2);
        assert_eq!(saved.backends["claude"].model, "claude-sonnet-5-5");
        let json = serde_json::to_string(&saved).unwrap();

        // A fresh session (say, after a restart) with only Claude connected.
        let log2 = Log::default();
        let claude = Scripted::new("claude", &log2, vec![say("Starting with ~/.cache.")]);
        let mut resumed = Session::with_backends(&orion, Mode::Auto, Some(claude), None);
        resumed.restore(serde_json::from_str(&json).unwrap());
        assert_eq!(resumed.id(), saved.id);
        assert_eq!(resumed.backend(Provider::Claude).unwrap().model(), "claude-sonnet-5-5");
        let history = resumed.backend(Provider::Claude).unwrap().export_history();
        assert_eq!(history, saved.backends["claude"].history, "Claude's own history comes back verbatim");

        // The follow-up goes to Claude, which answered last, without a re-sync.
        resumed.turn("ok, go ahead", &mut Recorder::default()).unwrap();
        assert_eq!(entries(&log2, "claude"), ["claude user: ok, go ahead"]);

        // The local model wasn't connected; its saved history is kept, not lost.
        let again = resumed.snapshot().unwrap();
        assert_eq!(again.backends["local"], saved.backends["local"]);
        assert_eq!(again.turns.len(), 3);
        assert_eq!(again.created_ms, saved.created_ms);
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
        let results: Vec<String> = log.lock().unwrap().iter().filter(|l| l.contains(" result: ")).cloned().collect();
        assert!(results[0].starts_with("claude result: ~/.ssh is private"), "{results:?}");
        // The local model gets past the guard (and then fails normally: no such file here).
        assert!(!results[1].contains("is private"), "{results:?}");
    }


    #[test]
    fn keeps_a_journal_and_recalls_it_when_it_matters() {
        let log = Log::default();
        let orion = orion();
        let home = tempfile::tempdir().unwrap();
        let write = ToolCall {
            id: "w".into(),
            name: "journal_write".into(),
            input: json!({ "topic": "backups", "text": "Backups run nightly with restic to the NAS at /mnt/nas/restic." }),
            invalid_input: None,
        };
        let local = Scripted::new("local", &log, vec![Ok(("Writing it down.".into(), vec![write])), say("done"), say("They ran."), say("Hi!")]);
        let mut s = Session::with_backends(&orion, Mode::Local, None, Some(local));
        s.set_journal_home(Some(home.path().into()));
        s.set_person("ana");
        assert!(s.system_for(Provider::Local).contains("Your journal for Ana is empty."));
        s.turn("note how the backups work", &mut Recorder::default()).unwrap();
        assert_eq!(entries(&log, "local result"), ["local result: ok"]);
        assert!(home.path().join(".grok/state/semantic/ana_backups/memories.jsonl").is_file());

        let mut ui = Recorder::default();
        s.turn("did the restic backups run last night?", &mut ui).unwrap();
        s.turn("hello there", &mut ui).unwrap();
        let sent = entries(&log, "local user");
        assert!(sent[1].contains("[From Orion's journal, matching this message:\n- (backups, today) Backups run nightly"), "{sent:?}");
        assert_eq!(sent[2], "local user: hello there", "nothing to recall");
        assert!(ui.out.contains("[recalled from the journal: 1]"), "{}", ui.out);
        // The transcript keeps the message as it was written.
        assert_eq!(s.turns()[1].user, "did the restic backups run last night?");
        // The next conversation's prompt names the topic.
        s.clear();
        assert!(s.system_for(Provider::Claude).contains("topics, most recent first: backups (1)"));
    }

    #[test]
    fn a_hosted_local_backend_gets_nothing_private() {
        let log = Log::default();
        let orion = Orion::new(
            crate::Policy::from_toml(
                "[chat.local]\nbase_url = \"https://generativelanguage.googleapis.com/v1beta/openai\"\n[audit]\nenabled = false\n",
            )
            .unwrap(),
        );
        let (memory_dir, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut notes = Memory::load(memory_dir.path(), "ana").unwrap();
        notes.add("Has a medical appointment on Tuesdays", true).unwrap();
        notes.add("Prefers metric units", false).unwrap();
        let journal = Journal::open(home.path(), "ana");
        journal.write("health", "The physiotherapy appointment for the knee moved to Thursdays.", true).unwrap();

        let read = ToolCall { id: "r".into(), name: "fs_read".into(), input: json!({ "path": "~/.ssh/id_ed25519" }), invalid_input: None };
        let local = Scripted::new("local", &log, vec![Ok((String::new(), vec![read])), say("Blocked.")]);
        let claude = Scripted::new("claude", &log, vec![]);
        let mut s = Session::with_backends(&orion, Mode::Auto, Some(claude), Some(local));
        s.set_memory_dir(Some(memory_dir.path().into()));
        s.set_journal_home(Some(home.path().into()));
        s.set_person("ana");
        let prompt = s.system_for(Provider::Local);
        assert!(prompt.contains("Prefers metric units") && !prompt.contains("medical"), "{prompt}");

        // A private message would go to the local model, but it's hosted: ask.
        let mut ui = Recorder::default();
        s.turn("my bank password is hunter2", &mut ui).unwrap();
        assert!(ui.out.contains("!Not sent."), "{}", ui.out);
        assert!(s.explain_route("my bank password").contains("no model on this machine"));

        s.turn("@local when is the knee physiotherapy?", &mut ui).unwrap();
        let sent = entries(&log, "local user");
        assert!(!sent[0].contains("Thursdays"), "private entries stay here: {sent:?}");
        let results = entries(&log, "local result");
        assert!(results[0].starts_with("local result: ~/.ssh is private"), "{results:?}");
    }
}
