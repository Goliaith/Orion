//! `orion shell`: the graphical shell. A local web server, on loopback
//! only and behind a per-run token, serves a single-page interface for the
//! same conversation thread the full-screen chat uses. Open it in any
//! browser, or boot into it as a kiosk.
//!
//! Every connected page sees the same live conversation: events are
//! broadcast as Server-Sent Events, and the conversation so far is
//! replayed to pages that connect (or reload) later.

mod http;

use std::collections::HashMap;
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use self::http::Request;
use super::agent::{self, capitalize, Answer, FromAgent, Status, ToAgent};
use super::machine::{watch_machine, Machine};
use super::store::{self, Store};
use super::terminal::{provider_label, Saver};
use super::Session;
use crate::voice::{is_stop_request, spoken_answer, Person, Recording, VoiceIo, WakeEvent, WakeState};
use crate::{interrupt, Orion};

const PAGE: &str = include_str!("shell.html");
const COOKIE: &str = "orion_token";

/// `[shell]` in orion.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShellConfig {
    /// Port on 127.0.0.1; 0 picks a free one.
    pub port: u16,
    /// Open the shell in the default browser when it starts.
    pub open_browser: bool,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self { port: 7310, open_browser: false }
    }
}

enum Pending {
    Approve(Sender<Answer>),
    Confirm(Sender<bool>),
}

/// Shared state between the conversation, the pages and voice.
struct Hub {
    token: String,
    port: u16,
    to_agent: Mutex<Sender<ToAgent>>,
    /// Open event streams.
    clients: Mutex<Vec<SyncSender<String>>>,
    /// Conversation events since the last transcript, replayed on connect.
    log: Mutex<Vec<String>>,
    status: Mutex<Option<String>>,
    machine: Mutex<Option<String>>,
    busy: AtomicBool,
    pending: Mutex<HashMap<u64, Pending>>,
    next_id: AtomicU64,
    voice: Mutex<VoiceIo>,
    listening: Mutex<Option<Recording>>,
    transcribing: AtomicBool,
    /// A spoken request heard while the AI was busy, sent when it is done.
    wake_queue: Mutex<Option<(String, Option<Person>)>>,
    /// Why wake listening didn't start, for pages that connect.
    wake_error: Mutex<Option<String>>,
    show_reasoning: bool,
}

impl Hub {
    /// Send an event to every page; `keep` adds it to the replay log.
    fn broadcast(&self, ev: Value, keep: bool) {
        let text = ev.to_string();
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        if ev["type"] == "transcript" {
            log.clear();
        }
        if keep || ev["type"] == "transcript" {
            log.push(text.clone());
        }
        // Holding the log lock while sending keeps replay and live events in order.
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        clients.retain(|c| c.try_send(text.clone()).is_ok());
    }

    fn set_busy(&self, on: bool) {
        self.busy.store(on, Ordering::SeqCst);
        self.broadcast(json!({ "type": "busy", "on": on }), false);
    }

    fn send_agent(&self, msg: ToAgent) {
        let _ = self.to_agent.lock().unwrap_or_else(|e| e.into_inner()).send(msg);
    }

    fn voice<T>(&self, f: impl FnOnce(&mut VoiceIo) -> T) -> T {
        f(&mut self.voice.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Send a message as the user, as if typed; as spoken by an enrolled
    /// person if `by` is given.
    fn start_turn(&self, text: String, by: Option<Person>) {
        self.voice(|v| v.hush());
        self.set_busy(true);
        match by {
            Some(by) => {
                let shown = format!("{} (by voice): {text}", capitalize(&by.name));
                self.broadcast(json!({ "type": "user", "text": shown }), true);
                self.send_agent(ToAgent::Spoken { text, by });
            }
            None => {
                self.broadcast(json!({ "type": "user", "text": text }), true);
                self.send_agent(ToAgent::Turn(text));
            }
        }
    }

    fn wake_json(&self) -> Value {
        self.voice(|v| {
            json!({
                "type": "wake",
                "on": v.wake().is_some(),
                "phrase": v.config.wake_phrase.trim(),
                "state": match v.wake().map(|w| w.state()) {
                    Some(WakeState::Hearing) => "hearing",
                    Some(WakeState::Transcribing) => "transcribing",
                    _ => "waiting",
                },
            })
        })
    }
}

fn status_json(s: &Status) -> Value {
    json!({
        "type": "status",
        "id": s.id,
        "mode": super::terminal::mode_label(s.mode),
        "routing": s.routing.trim_start_matches("routing: "),
        "providers": s.providers,
        "usage": s.usage.iter().map(|(label, u)| json!({
            "label": label,
            "input": u.input_tokens,
            "output": u.output_tokens,
            "cached": u.cache_read_input_tokens,
        })).collect::<Vec<_>>(),
        "title": s.title,
        "turns": s.turns,
        "person": capitalize(&s.person),
        "notes": s.notes,
        "tools_auto": s.tools_auto,
        "tools_ask": s.tools_ask,
    })
}

/// Turn what the conversation reports into page events.
fn relay(hub: &Hub, from_agent: Receiver<FromAgent>) {
    for ev in from_agent {
        match ev {
            FromAgent::Text(t) => {
                hub.voice(|v| v.reply_text(&t));
                hub.broadcast(json!({ "type": "text", "text": t }), true);
            }
            FromAgent::Thinking(t) => hub.broadcast(json!({ "type": "thinking", "text": t }), true),
            FromAgent::Route { to, model, reason } => {
                let who = if to == "Claude" { "claude" } else { "local" };
                hub.broadcast(json!({ "type": "route", "to": to, "who": who, "model": model, "reason": reason }), true)
            }
            FromAgent::ToolStart { name, args } => {
                hub.broadcast(json!({ "type": "tool_start", "name": name, "args": args }), true)
            }
            FromAgent::ToolEnd { ok, detail, took } => hub.broadcast(
                json!({ "type": "tool_end", "ok": ok, "detail": detail, "ms": took.as_millis() as u64 }),
                true,
            ),
            FromAgent::Notice(t) => hub.broadcast(json!({ "type": "notice", "text": t }), true),
            FromAgent::Error(t) => hub.broadcast(json!({ "type": "error", "text": t }), true),
            FromAgent::Output(t) => hub.broadcast(json!({ "type": "output", "text": t }), true),
            FromAgent::Approve { tool, preview, reply } => {
                let id = hub.next_id.fetch_add(1, Ordering::SeqCst);
                hub.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, Pending::Approve(reply));
                hub.broadcast(json!({ "type": "approve", "id": id, "tool": tool, "preview": preview }), true);
            }
            FromAgent::Confirm { question, reply } => {
                let id = hub.next_id.fetch_add(1, Ordering::SeqCst);
                hub.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, Pending::Confirm(reply));
                hub.broadcast(json!({ "type": "confirm", "id": id, "question": question }), true);
            }
            FromAgent::Transcript(turns) => {
                let turns: Vec<Value> = turns
                    .iter()
                    .map(|t| json!({ "user": t.user, "who": provider_label(t.provider), "reply": t.reply, "tools": t.tools }))
                    .collect();
                hub.broadcast(json!({ "type": "transcript", "turns": turns }), true);
            }
            FromAgent::Status(s) => {
                let v = status_json(&s);
                *hub.status.lock().unwrap_or_else(|e| e.into_inner()) = Some(v.to_string());
                hub.broadcast(v, false);
            }
            FromAgent::Done => {
                hub.voice(|v| v.reply_done());
                // Prompts left unanswered when a turn ends are moot.
                hub.pending.lock().unwrap_or_else(|e| e.into_inner()).clear();
                hub.set_busy(false);
                if let Some((text, by)) = hub.wake_queue.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    hub.start_turn(text, by);
                }
            }
            FromAgent::Quit => {}
        }
    }
}

/// Voice: the level meter and pause detector while listening, the
/// transcript when done, and whether Orion is speaking.
fn watch_voice(hub: &Hub, stop: &AtomicBool) {
    let mut was_speaking = false;
    let mut wake_was = Value::Null;
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(100));
        let finished = {
            let listening = hub.listening.lock().unwrap_or_else(|e| e.into_inner());
            match listening.as_ref() {
                Some(rec) => {
                    let ev = json!({ "type": "voice", "state": "listening", "level": rec.level(), "secs": rec.elapsed().as_secs() });
                    hub.broadcast(ev, false);
                    rec.should_stop()
                }
                None => false,
            }
        };
        if finished {
            finish_listening(hub);
        }
        watch_wake(hub, &mut wake_was);
        let speaking = hub.voice(|v| v.speaker.as_ref().is_ok_and(|s| s.is_speaking()));
        if speaking != was_speaking {
            was_speaking = speaking;
            hub.broadcast(json!({ "type": "speaking", "on": speaking }), false);
        }
    }
}

/// The wake listener: pause it while Ctrl-T style listening has the
/// microphone, pass on its state, and act on what it heard.
fn watch_wake(hub: &Hub, was: &mut Value) {
    let recording = hub.listening.lock().unwrap_or_else(|e| e.into_inner()).is_some() || hub.transcribing.load(Ordering::SeqCst);
    let (events, level) = hub.voice(|v| {
        if let Some(w) = v.wake() {
            w.set_paused(recording);
        }
        let level = v.wake().filter(|w| w.state() == WakeState::Hearing).map(|w| w.level());
        (std::iter::from_fn(|| v.wake_event()).collect::<Vec<_>>(), level)
    });
    let now = hub.wake_json();
    if now != *was || level.is_some() {
        let mut ev = now.clone();
        if let Some(l) = level {
            ev["level"] = json!(l);
        }
        hub.broadcast(ev, false);
        *was = now;
    }
    for ev in events {
        on_wake(hub, ev);
    }
}

fn on_wake(hub: &Hub, ev: WakeEvent) {
    let note = |text: &str| hub.broadcast(json!({ "type": "wake_note", "text": text }), false);
    match ev {
        WakeEvent::Woke => {
            hub.voice(|v| v.hush());
        }
        WakeEvent::NoRequest => note("Woke, but heard no request"),
        WakeEvent::NotYou => note("That didn't sound like your voice"),
        WakeEvent::Failed(e) => {
            hub.broadcast(json!({ "type": "voice_error", "text": format!("stopped listening for the wake phrase: {e}") }), false);
            hub.broadcast(hub.wake_json(), false);
        }
        WakeEvent::Request { text, speaker } => {
            hub.voice(|v| v.hush());
            let approvals = hub.voice(|v| v.config.may_approve(speaker.as_deref()));
            let by = speaker.as_deref().map(|n| hub.voice(|v| v.config.who(n)));
            let pending = {
                let mut p = hub.pending.lock().unwrap_or_else(|e| e.into_inner());
                match spoken_answer(&text) {
                    Some(yes) if !p.is_empty() && (approvals || !yes) => {
                        let id = *p.keys().max().unwrap_or(&0);
                        p.remove(&id).map(|prompt| (id, prompt, yes))
                    }
                    _ if !p.is_empty() => {
                        drop(p);
                        return note(&if spoken_answer(&text) == Some(true) {
                            match &speaker {
                                Some(n) if hub.voice(|v| v.config.wake_approvals) => {
                                    format!("{} may not approve by voice (voice.people.{n})", capitalize(n))
                                }
                                _ => "Approve with the button: spoken approval is off (voice.wake_approvals)".to_string(),
                            }
                        } else {
                            format!("Heard \"{text}\" · answer the prompt first")
                        });
                    }
                    _ => None,
                }
            };
            if let Some((id, prompt, yes)) = pending {
                match prompt {
                    Pending::Approve(tx) => {
                        let _ = tx.send(if yes { Answer::Yes } else { Answer::No });
                    }
                    Pending::Confirm(tx) => {
                        let _ = tx.send(yes);
                    }
                }
                hub.broadcast(json!({ "type": "answered", "id": id, "answer": if yes { "yes" } else { "no" } }), true);
            } else if is_stop_request(&text) {
                if hub.busy.load(Ordering::SeqCst) {
                    interrupt::request();
                }
            } else if hub.busy.load(Ordering::SeqCst) {
                *hub.wake_queue.lock().unwrap_or_else(|e| e.into_inner()) = Some((text.clone(), by));
                note(&format!("Queued \"{text}\": it sends when the AI is done"));
            } else {
                hub.start_turn(text, by);
            }
        }
    }
}

fn finish_listening(hub: &Hub) {
    let Some(rec) = hub.listening.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
    let Ok(transcriber) = hub.voice(|v| v.transcriber.clone()) else { return };
    hub.transcribing.store(true, Ordering::SeqCst);
    hub.broadcast(json!({ "type": "voice", "state": "transcribing" }), false);
    let result = rec.finish().and_then(|wav| transcriber.transcribe(&wav));
    hub.transcribing.store(false, Ordering::SeqCst);
    hub.broadcast(json!({ "type": "voice", "state": "idle" }), false);
    match result {
        Ok(text) => hub.broadcast(json!({ "type": "heard", "text": text }), false),
        Err(e) => hub.broadcast(json!({ "type": "voice_error", "text": format!("{e:#}") }), false),
    }
}

/// 128 random bits as hex.
fn random_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Constant-time comparison, so the token can't be guessed byte by byte.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Run the shell until the process is stopped.
pub(super) fn run(orion: &Orion, session: Session, saver: Saver, port: Option<u16>, open: bool) -> Result<()> {
    let config = &orion.policy().shell;
    let listener = TcpListener::bind(("127.0.0.1", port.unwrap_or(config.port)))
        .with_context(|| format!("listening on 127.0.0.1:{}", port.unwrap_or(config.port)))?;
    let port = listener.local_addr()?.port();
    let token = random_token()?;
    let url = format!("http://127.0.0.1:{port}/?token={token}");

    // Where a kiosk launcher can find the address.
    if let Some(dir) = crate::Policy::state_dir() {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("shell-url");
        if let Ok(mut f) = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path) {
            use std::io::Write;
            let _ = writeln!(f, "{url}");
        }
    }
    println!("Orion shell running at\n\n  {url}\n\nOpen it in a browser on this machine. Ctrl-C stops the shell.");
    if open || config.open_browser {
        let opener = std::env::var("BROWSER").unwrap_or_else(|_| "xdg-open".into());
        let _ = std::process::Command::new(opener).arg(&url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
    }

    let (to_agent, agent_rx) = mpsc::channel();
    let (agent_tx, from_agent) = mpsc::channel();
    let (machine_tx, machine_rx) = mpsc::channel::<Machine>();
    let stop = AtomicBool::new(false);
    let mut voice = VoiceIo::new(&orion.policy().voice);
    let wake_error = voice.support_wake().err();
    let hub = Arc::new(Hub {
        token,
        port,
        to_agent: Mutex::new(to_agent),
        clients: Mutex::new(Vec::new()),
        log: Mutex::new(Vec::new()),
        status: Mutex::new(None),
        machine: Mutex::new(None),
        busy: AtomicBool::new(true),
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        voice: Mutex::new(voice),
        listening: Mutex::new(None),
        wake_queue: Mutex::new(None),
        wake_error: Mutex::new(wake_error),
        transcribing: AtomicBool::new(false),
        show_reasoning: orion.policy().chat.show_reasoning,
    });

    thread::scope(|scope| {
        scope.spawn(move || agent::run(orion, session, saver, agent_rx, agent_tx));
        scope.spawn(|| relay(&hub, from_agent));
        scope.spawn(|| watch_machine(orion.policy(), machine_tx, &stop));
        scope.spawn(|| {
            for m in machine_rx {
                let v = json!({ "type": "machine", "machine": m });
                *hub.machine.lock().unwrap_or_else(|e| e.into_inner()) = Some(v.to_string());
                hub.broadcast(v, false);
            }
        });
        scope.spawn(|| watch_voice(&hub, &stop));
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let hub = hub.clone();
            thread::spawn(move || {
                if let Ok(req) = http::read_request(&stream) {
                    let _ = handle(&hub, &stream, req);
                }
            });
        }
        stop.store(true, Ordering::Relaxed);
    });
    Ok(())
}

fn handle(hub: &Arc<Hub>, stream: &TcpStream, req: Request) -> std::io::Result<()> {
    // The Host header must name this server, which defeats DNS rebinding.
    let host_ok = req.header("host").is_some_and(|h| {
        [format!("127.0.0.1:{}", hub.port), format!("localhost:{}", hub.port), format!("[::1]:{}", hub.port)].contains(&h.to_string())
    });
    if !host_ok {
        return http::respond(stream, 403, "text/plain", b"wrong host", &[]);
    }
    let by_cookie = req.cookie(COOKIE).is_some_and(|t| same(t, &hub.token));
    let by_query = req.query.get("token").is_some_and(|t| same(t, &hub.token));

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") if by_query => {
            // Swap the token in the address for a cookie, and drop it from the URL.
            let cookie = format!("{COOKIE}={}; HttpOnly; SameSite=Strict; Path=/", hub.token);
            http::respond(stream, 303, "text/plain", b"", &[("set-cookie", cookie), ("location", "/".into())])
        }
        ("GET", "/") if by_cookie => {
            let csp = "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'".to_string();
            http::respond(stream, 200, "text/html; charset=utf-8", PAGE.as_bytes(), &[("content-security-policy", csp)])
        }
        ("GET", "/") => http::respond(
            stream,
            401,
            "text/html; charset=utf-8",
            b"<!doctype html><title>Orion</title><body style=\"font:16px system-ui;padding:3rem\">Open the address that <code>orion shell</code> printed (it is also in <code>~/.local/state/orion/shell-url</code>).",
            &[],
        ),
        _ if !by_cookie => http::respond(stream, 401, "text/plain", b"unauthorized", &[]),
        ("GET", "/api/events") => events(hub, stream),
        ("GET", "/api/conversations") => {
            let list = Store::default_location().and_then(|s| s.list().ok()).unwrap_or_default();
            let list: Vec<Value> = list
                .iter()
                .map(|c| json!({ "id": c.id, "title": c.title, "turns": c.turns, "ago": store::ago(c.updated_ms) }))
                .collect();
            http::json(stream, 200, &json!(list))
        }
        // Changes need a header a cross-site page can't send without a CORS
        // preflight, which this server never approves.
        ("POST", _) if req.header("x-orion") != Some("1") => http::respond(stream, 403, "text/plain", b"forbidden", &[]),
        ("POST", path) => {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let (status, reply) = post(hub, path, &body);
            http::json(stream, status, &reply)
        }
        _ => http::respond(stream, 404, "text/plain", b"not found", &[]),
    }
}

fn post(hub: &Arc<Hub>, path: &str, body: &Value) -> (u16, Value) {
    let busy = hub.busy.load(Ordering::SeqCst);
    let ok = json!({ "ok": true });
    match path {
        "/api/send" => {
            let text = body["text"].as_str().unwrap_or_default().trim().to_string();
            if text.is_empty() {
                return (400, json!({ "error": "empty message" }));
            }
            if busy {
                return (409, json!({ "error": "busy" }));
            }
            hub.voice(|v| v.hush());
            if let Some(arg) = text.strip_prefix("/voice") {
                let out = hub.voice(|v| v.command(arg.trim()));
                hub.broadcast(json!({ "type": "output", "text": format!("{text}\n{out}") }), true);
                hub.broadcast(json!({ "type": "speak_replies", "on": hub.voice(|v| v.speak_replies) }), false);
                hub.broadcast(hub.wake_json(), false);
                return (200, ok);
            }
            if matches!(text.as_str(), "/exit" | "/quit" | "/q") {
                hub.broadcast(json!({ "type": "notice", "text": "The shell keeps running; close the window to leave, or stop it with Ctrl-C where it runs." }), true);
                return (200, ok);
            }
            hub.set_busy(true);
            match text.strip_prefix('/') {
                Some(cmd) => {
                    hub.broadcast(json!({ "type": "output", "text": text }), true);
                    hub.send_agent(ToAgent::Command(cmd.to_string()));
                }
                None => {
                    hub.broadcast(json!({ "type": "user", "text": text }), true);
                    hub.send_agent(ToAgent::Turn(text));
                }
            }
            (200, ok)
        }
        "/api/stop" => {
            hub.voice(|v| v.hush());
            if busy {
                interrupt::request();
            }
            (200, ok)
        }
        "/api/answer" => {
            let id = body["id"].as_u64().unwrap_or(0);
            let answer = body["answer"].as_str().unwrap_or("no");
            let Some(p) = hub.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) else {
                return (409, json!({ "error": "already answered" }));
            };
            match p {
                Pending::Approve(tx) => {
                    let a = match answer {
                        "yes" => Answer::Yes,
                        "always" => Answer::Always,
                        _ => Answer::No,
                    };
                    let _ = tx.send(a);
                }
                Pending::Confirm(tx) => {
                    let _ = tx.send(answer == "yes");
                }
            }
            hub.broadcast(json!({ "type": "answered", "id": id, "answer": answer }), true);
            (200, ok)
        }
        "/api/voice" => match body["action"].as_str().unwrap_or_default() {
            "start" => {
                if hub.transcribing.load(Ordering::SeqCst) {
                    return (409, json!({ "error": "still transcribing" }));
                }
                let started = hub.voice(|v| {
                    v.hush();
                    v.transcriber.as_ref().map_err(|e| format!("can't transcribe: {e}"))?;
                    v.recorder.as_ref().map_err(String::clone)?.start().map_err(|e| format!("{e:#}"))
                });
                match started {
                    Ok(rec) => {
                        *hub.listening.lock().unwrap_or_else(|e| e.into_inner()) = Some(rec);
                        (200, ok)
                    }
                    Err(e) => (400, json!({ "error": e })),
                }
            }
            "stop" => {
                // Transcribing can take a while; the result arrives as an event.
                let hub = hub.clone();
                thread::spawn(move || finish_listening(&hub));
                (200, ok)
            }
            "wake_on" => match hub.voice(|v| v.wake_on()) {
                Ok(_) => {
                    hub.broadcast(hub.wake_json(), false);
                    (200, ok)
                }
                Err(e) => (400, json!({ "error": e })),
            },
            "wake_off" => {
                hub.voice(|v| v.wake_off());
                *hub.wake_queue.lock().unwrap_or_else(|e| e.into_inner()) = None;
                hub.broadcast(hub.wake_json(), false);
                (200, ok)
            }
            "cancel" => {
                if let Some(rec) = hub.listening.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    rec.cancel();
                }
                hub.broadcast(json!({ "type": "voice", "state": "idle" }), false);
                (200, ok)
            }
            _ => (400, json!({ "error": "unknown action" })),
        },
        _ => (404, json!({ "error": "not found" })),
    }
}

/// One page's event stream: catch it up, then follow live.
fn events(hub: &Hub, stream: &TcpStream) -> std::io::Result<()> {
    http::start_events(stream)?;
    let (tx, rx) = mpsc::sync_channel::<String>(1024);
    let replay = {
        let log = hub.log.lock().unwrap_or_else(|e| e.into_inner());
        hub.clients.lock().unwrap_or_else(|e| e.into_inner()).push(tx);
        log.clone()
    };
    let speak_replies = hub.voice(|v| v.speak_replies);
    let mic = hub.voice(|v| v.recorder.is_ok() && v.transcriber.is_ok());
    let speech = hub.voice(|v| v.speaker.is_ok());
    let wake_error = hub.wake_error.lock().unwrap_or_else(|e| e.into_inner()).take();
    let hello = json!({ "type": "hello", "show_reasoning": hub.show_reasoning, "mic": mic, "speech": speech,
                        "speak_replies": speak_replies, "wake_error": wake_error });
    http::event(stream, &hello.to_string())?;
    http::event(stream, &hub.wake_json().to_string())?;
    for snapshot in [&hub.status, &hub.machine] {
        if let Some(s) = snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            http::event(stream, &s)?;
        }
    }
    for e in replay {
        http::event(stream, &e)?;
    }
    http::event(stream, &json!({ "type": "busy", "on": hub.busy.load(Ordering::SeqCst) }).to_string())?;
    http::event(stream, &json!({ "type": "replayed" }).to_string())?;
    loop {
        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(e) => http::event(stream, &e)?,
            // Also notices a closed page: the write fails.
            Err(RecvTimeoutError::Timeout) => http::keepalive(stream)?,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::voice::VoiceConfig;

    fn hub(port: u16) -> (Arc<Hub>, Receiver<ToAgent>) {
        let (tx, rx) = mpsc::channel();
        let hub = Hub {
            token: "a".repeat(32),
            port,
            to_agent: Mutex::new(tx),
            clients: Mutex::new(Vec::new()),
            log: Mutex::new(Vec::new()),
            status: Mutex::new(None),
            machine: Mutex::new(None),
            busy: AtomicBool::new(false),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            voice: Mutex::new(VoiceIo::new(&VoiceConfig { enabled: false, ..Default::default() })),
            listening: Mutex::new(None),
            transcribing: AtomicBool::new(false),
            wake_queue: Mutex::new(None),
            wake_error: Mutex::new(None),
            show_reasoning: true,
        };
        (Arc::new(hub), rx)
    }

    /// Send a raw request to `handle` and return the status code and body.
    fn request(hub: &Arc<Hub>, raw: &str) -> (u16, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let raw = raw.to_string();
        let client = thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(raw.as_bytes()).unwrap();
            let mut out = String::new();
            let _ = s.read_to_string(&mut out);
            out
        });
        let (stream, _) = listener.accept().unwrap();
        let req = http::read_request(&stream).unwrap();
        handle(hub, &stream, req).unwrap();
        drop(stream);
        let reply = client.join().unwrap();
        let status = reply.split_whitespace().nth(1).unwrap_or("0").parse().unwrap();
        (status, reply.split("\r\n\r\n").nth(1).unwrap_or_default().to_string())
    }

    #[test]
    fn guards_every_door() {
        let (hub, agent) = hub(7310);
        let host = "Host: 127.0.0.1:7310\r\n";
        let cookie = format!("Cookie: {COOKIE}={}\r\n", "a".repeat(32));
        let post = |extra: &str, body: &str| {
            format!("POST /api/send HTTP/1.1\r\n{host}{extra}Content-Length: {}\r\n\r\n{body}", body.len())
        };

        // DNS rebinding: a foreign Host is refused, even with the token.
        assert_eq!(request(&hub, &format!("GET /?token={} HTTP/1.1\r\nHost: evil.example:7310\r\n\r\n", "a".repeat(32))).0, 403);
        // No token, wrong token: no page, no API.
        assert_eq!(request(&hub, &format!("GET / HTTP/1.1\r\n{host}\r\n")).0, 401);
        assert_eq!(request(&hub, &format!("GET /?token=bbbb HTTP/1.1\r\n{host}\r\n")).0, 401);
        assert_eq!(request(&hub, &format!("GET /api/conversations HTTP/1.1\r\n{host}\r\n")).0, 401);
        // The token in the URL becomes a cookie.
        let (status, _) = request(&hub, &format!("GET /?token={} HTTP/1.1\r\n{host}\r\n", "a".repeat(32)));
        assert_eq!(status, 303);
        assert_eq!(request(&hub, &format!("GET / HTTP/1.1\r\n{host}{cookie}\r\n")).0, 200);
        // Changes need the custom header, which cross-site pages can't send.
        assert_eq!(request(&hub, &post(&cookie, r#"{"text":"hi"}"#)).0, 403);
        assert!(agent.try_recv().is_err());
        let (status, _) = request(&hub, &post(&format!("{cookie}X-Orion: 1\r\n"), r#"{"text":"hi"}"#));
        assert_eq!(status, 200);
        assert!(matches!(agent.try_recv().unwrap(), ToAgent::Turn(t) if t == "hi"));
        // One thing at a time.
        assert_eq!(request(&hub, &post(&format!("{cookie}X-Orion: 1\r\n"), r#"{"text":"again"}"#)).0, 409);
    }

    #[test]
    fn approvals_are_answered_once() {
        let (hub, _agent) = hub(7310);
        let (tx, rx) = mpsc::channel();
        hub.pending.lock().unwrap().insert(5, Pending::Approve(tx));
        let cookie = format!("Cookie: {COOKIE}={}\r\nX-Orion: 1\r\n", "a".repeat(32));
        let body = r#"{"id":5,"answer":"always"}"#;
        let raw = format!("POST /api/answer HTTP/1.1\r\nHost: localhost:7310\r\n{cookie}Content-Length: {}\r\n\r\n{body}", body.len());
        assert_eq!(request(&hub, &raw).0, 200);
        assert_eq!(rx.try_recv().unwrap(), Answer::Always);
        assert_eq!(request(&hub, &raw).0, 409, "a second answer (another tab) is refused");
    }

    #[test]
    fn tokens() {
        let t = random_token().unwrap();
        assert_eq!(t.len(), 32);
        assert_ne!(t, random_token().unwrap());
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
    }
}
