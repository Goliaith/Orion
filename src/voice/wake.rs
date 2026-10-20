//! Wake-word listening: say "Orion, …" and it hears you, hands-free.
//!
//! The microphone stays open. A pause detector cuts the sound into
//! utterances, and the start of each one is transcribed (by the same local
//! transcriber as Ctrl-T) and checked for the wake phrase. "Orion, what's
//! using the GPU?" becomes that request at once; a bare "Orion" plays a
//! chime and listens for the request. Audio is never kept: each utterance
//! is dropped once it is checked.
//!
//! Instead of transcribing, `voice.wake_command` can name a dedicated
//! wake-word detector (openWakeWord, Porcupine, …) that prints a line each
//! time it hears the word; Orion then records the request itself.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use super::record::{meter, rms, speech_threshold, CHUNK_MS, MIN_SPEECH_MS};
use super::speaker::{Judge, Verdict};
use super::{kill_group, shell, Recorder, Transcriber, VoiceConfig};

/// The part of an utterance checked for the wake phrase.
const CHECK_MS: u64 = 3000;
/// A pause this long ends an utterance while waiting for the wake word.
const GAP_MS: u64 = 600;
/// Utterances are cut at this length while waiting for the wake word.
const MAX_UTTERANCE_MS: u64 = 8000;
/// Sound kept from just before speech starts, so its first syllable isn't lost.
const PRE_ROLL_CHUNKS: usize = 3;

/// Words people say before the wake phrase.
const FILLERS: [&str; 9] = ["hey", "hi", "ok", "okay", "yo", "um", "uh", "so", "oh"];

#[derive(Debug, Clone, PartialEq)]
pub enum WakeEvent {
    /// The wake phrase alone: now listening for the request.
    Woke,
    /// What was asked, without the wake phrase.
    Request(String),
    /// Woke, but no request followed.
    NoRequest,
    /// Woke by a bare wake phrase, but the request was in another voice
    /// than the enrolled one.
    NotYou,
    /// Listening stopped (the microphone failed, say).
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WakeState {
    /// Waiting for the wake phrase.
    Waiting,
    /// Woke; recording the request.
    Hearing,
    Transcribing,
    /// Not listening for now (Ctrl-T has the microphone).
    Paused,
    Stopped,
}

struct Shared {
    stop: AtomicBool,
    paused: AtomicBool,
    state: Mutex<WakeState>,
    level: Mutex<f32>,
    /// The process group holding the microphone, to stop it.
    child: Mutex<Option<u32>>,
    /// Utterances dropped, unheard, for being in another voice.
    ignored: std::sync::atomic::AtomicUsize,
}

impl Shared {
    fn set(&self, s: WakeState) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = s;
    }
}

pub struct WakeListener {
    shared: Arc<Shared>,
    events: Receiver<WakeEvent>,
    description: String,
}

/// Everything the listening thread needs.
struct Setup {
    phrase: String,
    aliases: Vec<String>,
    recorder: Recorder,
    transcriber: Transcriber,
    command: String,
    /// Only this voice, at this threshold.
    voice: Option<Judge>,
    chime: Option<String>,
    silence_ms: u64,
    max_ms: u64,
    timeout: Duration,
}

impl WakeListener {
    /// Listen for the wake phrase; with a voiceprint, only in that voice.
    pub fn start(config: &VoiceConfig, recorder: Recorder, transcriber: Transcriber, voice: Option<Judge>) -> Result<Self> {
        let phrase = config.wake_phrase.trim().to_lowercase();
        if words(&phrase).is_empty() {
            bail!("voice.wake_phrase is empty");
        }
        if config.wake_command.is_empty() && !transcriber.is_local() && !config.wake_allow_remote {
            bail!(
                "wake listening would send everything said near the microphone to {} for transcription; \
                 use a transcriber on this machine, a wake_command, or set voice.wake_allow_remote",
                transcriber.describe()
            );
        }
        let description = if config.wake_command.is_empty() {
            format!("say \"{}\" (heard by {})", config.wake_phrase.trim(), transcriber.describe())
        } else {
            format!("say the wake word (detected by {})", config.wake_command.split_whitespace().next().unwrap_or_default())
        };
        let description = if voice.is_some() { format!("{description}, only your voice") } else { description };
        let setup = Setup {
            phrase,
            aliases: config.wake_aliases.iter().map(|a| a.to_lowercase()).collect(),
            recorder,
            transcriber,
            command: config.wake_command.clone(),
            voice,
            chime: config.wake_chime.then(|| super::player(config)).flatten(),
            silence_ms: config.silence_ms.max(300),
            max_ms: config.max_seconds.max(1) * 1000,
            timeout: Duration::from_secs(config.wake_timeout_secs.max(1)),
        };
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            state: Mutex::new(WakeState::Waiting),
            level: Mutex::new(0.0),
            child: Mutex::new(None),
            ignored: Default::default(),
        });
        let (tx, events) = mpsc::channel();
        let sh = shared.clone();
        thread::spawn(move || {
            let result = if setup.command.is_empty() { listen(&setup, &sh, &tx) } else { detect(&setup, &sh, &tx) };
            sh.set(WakeState::Stopped);
            if let Err(e) = result {
                if !sh.stop.load(Ordering::SeqCst) {
                    let _ = tx.send(WakeEvent::Failed(format!("{e:#}")));
                }
            }
        });
        Ok(Self { shared, events, description })
    }

    pub fn describe(&self) -> &str {
        &self.description
    }

    /// Stop listening for a while (another recording has the microphone).
    pub fn set_paused(&self, on: bool) {
        self.shared.paused.store(on, Ordering::SeqCst);
    }

    pub fn state(&self) -> WakeState {
        let s = *self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if s == WakeState::Waiting && self.shared.paused.load(Ordering::SeqCst) {
            WakeState::Paused
        } else {
            s
        }
    }

    /// Input level from 0 to 1, for a meter while hearing a request.
    pub fn level(&self) -> f32 {
        *self.shared.level.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How many utterances were dropped for being in another voice.
    pub fn ignored(&self) -> usize {
        self.shared.ignored.load(Ordering::SeqCst)
    }

    pub fn try_event(&self) -> Option<WakeEvent> {
        self.events.try_recv().ok()
    }
}

impl Drop for WakeListener {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(pid) = self.shared.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            kill_group(pid, libc::SIGKILL);
        }
    }
}

/// Holds a child process's group for the listener, and kills it on drop.
struct Held<'a>(&'a Shared, Child);

impl Drop for Held<'_> {
    fn drop(&mut self) {
        kill_group(self.1.id(), libc::SIGKILL);
        let _ = self.1.wait();
        self.0.child.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

fn hold(shared: &Shared, child: Child) -> Held<'_> {
    *shared.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child.id());
    Held(shared, child)
}

/// The transcribing engine: one open microphone, cut into utterances.
fn listen(setup: &Setup, shared: &Shared, tx: &Sender<WakeEvent>) -> Result<()> {
    let rate = setup.recorder.rate();
    let mut held = hold(shared, setup.recorder.spawn_tied()?);
    let stdout = held.1.stdout.take().context("the recorder has no output")?;
    let chunks = pcm_chunks(stdout, (rate as u64 * CHUNK_MS / 1000) as usize);
    let mut seg = Segmenter::new(rate, GAP_MS, MAX_UTTERANCE_MS);
    // When woke by a bare wake phrase: since when we wait for the request.
    let mut hearing: Option<Instant> = None;
    // That wake phrase's audio, to judge the voice together with the request.
    let mut wake_audio: Vec<i16> = Vec::new();
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let chunk = match chunks.recv_timeout(Duration::from_millis(100)) {
            Ok(c) => c,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                let status = held.1.wait().ok();
                bail!("the microphone stopped{}", status.map(|s| format!(" (the recorder exited with {s})")).unwrap_or_default());
            }
        };
        if shared.paused.load(Ordering::SeqCst) {
            seg.reset();
            if hearing.take().is_some() {
                shared.set(WakeState::Waiting);
            }
            continue;
        }
        let done = seg.push(&chunk);
        *shared.level.lock().unwrap_or_else(|e| e.into_inner()) = seg.level;
        if let Some(since) = hearing {
            if !seg.in_speech() && since.elapsed() > setup.timeout {
                hearing = None;
                seg.set_limits(GAP_MS, MAX_UTTERANCE_MS);
                shared.set(WakeState::Waiting);
                let _ = tx.send(WakeEvent::NoRequest);
                continue;
            }
        }
        for utterance in done {
            if hearing.take().is_some() {
                // The request that follows a bare wake phrase.
                seg.set_limits(GAP_MS, MAX_UTTERANCE_MS);
                let both: Vec<i16> = std::mem::take(&mut wake_audio).into_iter().chain(utterance.iter().copied()).collect();
                if !verify(setup, shared, &both, rate, true) {
                    shared.set(WakeState::Waiting);
                    let _ = tx.send(WakeEvent::NotYou);
                    continue;
                }
                shared.set(WakeState::Transcribing);
                let heard = transcribe(setup, &utterance, rate);
                shared.set(WakeState::Waiting);
                match heard {
                    Some(text) => {
                        let _ = tx.send(WakeEvent::Request(text));
                    }
                    None => {
                        let _ = tx.send(WakeEvent::NoRequest);
                    }
                }
                continue;
            }
            // Another voice is dropped before anything transcribes it. Too
            // short to tell (a bare "Orion") may wake, and is judged again
            // together with the request.
            let unsure = match setup.voice.as_ref().map(|j| j.verify(&utterance, rate)) {
                Some(Verdict::Other(score)) => {
                    tracing::debug!("wake: another voice ({score})");
                    shared.ignored.fetch_add(1, Ordering::SeqCst);
                    continue;
                }
                Some(Verdict::TooShort) => true,
                _ => false,
            };
            let check = &utterance[..utterance.len().min((rate as u64 * CHECK_MS / 1000) as usize)];
            let Some(start) = transcribe(setup, check, rate) else { continue };
            let Some(rest) = match_wake(&start, &setup.phrase, &setup.aliases) else { continue };
            // Longer than the part checked: the request is in the rest of it.
            let rest = if check.len() < utterance.len() {
                transcribe(setup, &utterance, rate)
                    .and_then(|all| match_wake(&all, &setup.phrase, &setup.aliases))
                    .unwrap_or(rest)
            } else {
                rest
            };
            if has_words(&rest) && !unsure {
                let _ = tx.send(WakeEvent::Request(rest));
            } else if has_words(&rest) {
                // A request too short to tell the voice: not taken.
                shared.ignored.fetch_add(1, Ordering::SeqCst);
            } else {
                wake_audio = utterance;
                chime(setup);
                let _ = tx.send(WakeEvent::Woke);
                shared.set(WakeState::Hearing);
                seg.set_limits(setup.silence_ms, setup.max_ms);
                hearing = Some(Instant::now());
            }
        }
    }
}

/// The detector engine: an outside program says when it heard the word.
fn detect(setup: &Setup, shared: &Shared, tx: &Sender<WakeEvent>) -> Result<()> {
    let mut cmd = shell(&setup.command);
    cmd.stdout(Stdio::piped());
    super::die_with_parent(&mut cmd);
    let mut held = hold(shared, cmd.spawn().with_context(|| format!("starting {}", setup.command))?);
    let stdout = held.1.stdout.take().context("the wake detector has no output")?;
    let (line_tx, lines) = mpsc::channel::<()>();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if line.is_err() || line_tx.send(()).is_err() {
                break;
            }
        }
    });
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        match lines.recv_timeout(Duration::from_millis(100)) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                let status = held.1.wait().ok();
                bail!("the wake detector stopped{}", status.map(|s| format!(" ({s})")).unwrap_or_default());
            }
        }
        if shared.paused.load(Ordering::SeqCst) {
            continue;
        }
        chime(setup);
        let _ = tx.send(WakeEvent::Woke);
        shared.set(WakeState::Hearing);
        let event = record_request(setup, shared);
        // Detections while we were busy with this one are stale.
        while lines.try_recv().is_ok() {}
        shared.set(WakeState::Waiting);
        let _ = tx.send(event);
    }
}

/// After a detector's wake: record until the speaker pauses, then transcribe.
fn record_request(setup: &Setup, shared: &Shared) -> WakeEvent {
    let rec = match setup.recorder.start() {
        Ok(r) => r,
        Err(e) => return WakeEvent::Failed(format!("{e:#}")),
    };
    loop {
        *shared.level.lock().unwrap_or_else(|e| e.into_inner()) = rec.level();
        if shared.stop.load(Ordering::SeqCst) || shared.paused.load(Ordering::SeqCst) {
            rec.cancel();
            return WakeEvent::NoRequest;
        }
        if !rec.heard_speech() && rec.elapsed() > setup.timeout {
            rec.cancel();
            return WakeEvent::NoRequest;
        }
        if rec.should_stop() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let Ok(wav) = rec.finish() else { return WakeEvent::NoRequest };
    let samples: Vec<i16> = wav[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    if !verify(setup, shared, &samples, setup.recorder.rate(), true) {
        return WakeEvent::NotYou;
    }
    shared.set(WakeState::Transcribing);
    match setup.transcriber.transcribe(&wav) {
        Ok(text) if has_words(&text) => WakeEvent::Request(text),
        _ => WakeEvent::NoRequest,
    }
}

/// Whether the audio is in the enrolled voice (always, without one). With
/// `strict`, too short to tell counts as no.
fn verify(setup: &Setup, shared: &Shared, samples: &[i16], rate: u32, strict: bool) -> bool {
    let Some(judge) = &setup.voice else { return true };
    let ok = match judge.verify(samples, rate) {
        Verdict::Match(_) => true,
        Verdict::TooShort => !strict,
        Verdict::Other(score) => {
            tracing::debug!("wake: another voice ({score})");
            false
        }
    };
    if !ok {
        shared.ignored.fetch_add(1, Ordering::SeqCst);
    }
    ok
}

fn transcribe(setup: &Setup, samples: &[i16], rate: u32) -> Option<String> {
    match setup.transcriber.transcribe(&super::wav(samples, rate)) {
        Ok(text) => Some(text),
        Err(e) => {
            tracing::debug!("wake: {e:#}");
            None
        }
    }
}

/// Two short rising tones, played without waiting.
fn chime(setup: &Setup) {
    let Some(player) = &setup.chime else { return };
    let rate = 22_050;
    let tone = |freq: f32, ms: u32| {
        let n = rate * ms / 1000;
        (0..n).map(move |i| {
            let fade = (i.min(n - i) as f32 / (rate as f32 * 0.008)).min(1.0);
            ((i as f32 * freq * std::f32::consts::TAU / rate as f32).sin() * 6000.0 * fade) as i16
        })
    };
    let samples: Vec<i16> = tone(880.0, 90).chain(tone(1320.0, 120)).collect();
    let file = super::TempPath::new("wav");
    if std::fs::write(&file.0, super::wav(&samples, rate)).is_err() {
        return;
    }
    let command = player.replace("{wav}", &super::shell_quote(&file.0.to_string_lossy()));
    thread::spawn(move || {
        let _ = shell(&command).status();
        drop(file);
    });
}

/// Read a recorder's output as chunks of samples, on a thread of its own so
/// the pipe never fills while a transcription runs.
fn pcm_chunks(mut stdout: impl Read + Send + 'static, chunk: usize) -> Receiver<Vec<i16>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = vec![0u8; chunk * 2];
        let mut carry: Option<u8> = None;
        loop {
            let n = match stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let mut bytes: Vec<u8> = carry.take().into_iter().collect();
            bytes.extend_from_slice(&buf[..n]);
            if bytes.len() % 2 == 1 {
                carry = bytes.pop();
            }
            let samples = bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
            if tx.send(samples).is_err() {
                break;
            }
        }
    });
    rx
}

/// Cuts a stream of samples into utterances: speech, until a pause of
/// `gap_ms` or `max_ms` in all. The noise floor follows the room.
pub(crate) struct Segmenter {
    chunk: usize,
    pending: Vec<i16>,
    floor: Option<f32>,
    measured: u32,
    pre: VecDeque<Vec<i16>>,
    current: Option<Utterance>,
    gap_ms: u64,
    max_ms: u64,
    pub level: f32,
}

struct Utterance {
    samples: Vec<i16>,
    speech_ms: u64,
    silent_ms: u64,
    total_ms: u64,
}

impl Segmenter {
    pub fn new(rate: u32, gap_ms: u64, max_ms: u64) -> Self {
        Self {
            chunk: (rate as u64 * CHUNK_MS / 1000) as usize,
            pending: Vec::new(),
            floor: None,
            measured: 0,
            pre: VecDeque::new(),
            current: None,
            gap_ms,
            max_ms,
            level: 0.0,
        }
    }

    pub fn set_limits(&mut self, gap_ms: u64, max_ms: u64) {
        self.gap_ms = gap_ms;
        self.max_ms = max_ms;
    }

    pub fn reset(&mut self) {
        self.pending.clear();
        self.pre.clear();
        self.current = None;
    }

    pub fn in_speech(&self) -> bool {
        self.current.is_some()
    }

    /// Feed samples; returns the utterances they complete.
    pub fn push(&mut self, samples: &[i16]) -> Vec<Vec<i16>> {
        self.pending.extend_from_slice(samples);
        let mut out = Vec::new();
        while self.pending.len() >= self.chunk {
            let chunk: Vec<i16> = self.pending.drain(..self.chunk).collect();
            out.extend(self.measure(chunk));
        }
        out
    }

    fn measure(&mut self, chunk: Vec<i16>) -> Option<Vec<i16>> {
        let r = rms(&chunk);
        self.level = meter(r);
        self.measured += 1;
        let loud = self.measured > 3 && r > speech_threshold(self.floor);
        if !loud {
            // The quietest of the first chunks, then a slow follow of the room.
            self.floor = Some(match self.floor {
                None => r,
                Some(f) if self.measured <= 3 => f.min(r),
                Some(f) => f * 0.95 + r * 0.05,
            });
        }
        match &mut self.current {
            None if loud => {
                let mut samples: Vec<i16> = self.pre.drain(..).flatten().collect();
                samples.extend_from_slice(&chunk);
                self.current = Some(Utterance { samples, speech_ms: CHUNK_MS, silent_ms: 0, total_ms: CHUNK_MS });
                None
            }
            None => {
                self.pre.push_back(chunk);
                if self.pre.len() > PRE_ROLL_CHUNKS {
                    self.pre.pop_front();
                }
                None
            }
            Some(u) => {
                u.samples.extend_from_slice(&chunk);
                u.total_ms += CHUNK_MS;
                if loud {
                    u.speech_ms += CHUNK_MS;
                    u.silent_ms = 0;
                } else {
                    u.silent_ms += CHUNK_MS;
                }
                if u.silent_ms < self.gap_ms && u.total_ms < self.max_ms {
                    return None;
                }
                let u = self.current.take()?;
                (u.speech_ms >= MIN_SPEECH_MS).then_some(u.samples)
            }
        }
    }
}

/// Words with their byte spans: letters and digits, apostrophes dropped.
fn words(text: &str) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices().chain([(text.len(), ' ')]) {
        let part = c.is_alphanumeric() || c == '\'' || c == '’';
        match (start, part) {
            (None, true) => start = Some(i),
            (Some(s), false) => {
                let w: String = text[s..i].chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect();
                if !w.is_empty() {
                    out.push((w, s, i));
                }
                start = None;
            }
            _ => {}
        }
    }
    out
}

fn has_words(text: &str) -> bool {
    text.chars().filter(|c| c.is_alphanumeric()).count() >= 2
}

/// How it sounds, roughly: each run of vowels (and y) as one `a`, so
/// "Orion", "O'Ryan", "Oreon" and "Arion" all come out as "aran".
fn sound(word: &str) -> String {
    let mut out = String::new();
    for c in word.chars() {
        let c = if "aeiouy".contains(c) { 'a' } else { c };
        if !(c == 'a' && out.ends_with('a')) {
            out.push(c);
        }
    }
    out
}

fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut prev = row[0];
        row[0] = i;
        for j in 1..=b.len() {
            let cur = row[j];
            row[j] = (prev + usize::from(a[i - 1] != b[j - 1])).min(row[j] + 1).min(row[j - 1] + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// Whether `heard` is the wake phrase as a transcriber might spell it.
fn sounds_like(heard: &str, phrase: &str) -> bool {
    let close = (phrase.len() / 4).max(1);
    let d = edit_distance(heard, phrase);
    d <= close || (d <= close + 1 && sound(heard) == sound(phrase))
}

/// If `text` starts with the wake phrase (after a filler like "hey"), what
/// follows it, with leading punctuation trimmed (possibly empty).
pub fn match_wake(text: &str, phrase: &str, aliases: &[String]) -> Option<String> {
    let heard = words(text);
    let targets: Vec<String> = std::iter::once(phrase)
        .chain(aliases.iter().map(String::as_str))
        .map(|p| words(p).into_iter().map(|w| w.0).collect::<String>())
        .filter(|t| !t.is_empty())
        .collect();
    for skip in 0..=2.min(heard.len()) {
        if heard[..skip].iter().any(|w| !FILLERS.contains(&w.0.as_str())) {
            break;
        }
        for take in 1..=3 {
            let Some(window) = heard.get(skip..skip + take) else { break };
            let joined: String = window.iter().map(|w| w.0.as_str()).collect();
            if targets.iter().any(|t| sounds_like(&joined, t)) {
                let end = window.last()?.2;
                let rest = text[end..].trim_start_matches(|c: char| !c.is_alphanumeric()).trim_end();
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// "Stop", "never mind", "be quiet": a request to stop, not a question.
pub fn is_stop_request(text: &str) -> bool {
    let w: Vec<String> = words(text).into_iter().map(|w| w.0).collect();
    let w: Vec<&str> = w.iter().map(String::as_str).collect();
    matches!(
        w.as_slice(),
        ["stop"] | ["stop", "it"] | ["stop", "that"] | ["cancel"] | ["cancel", "that"] | ["never", "mind"]
            | ["nevermind"] | ["quiet"] | ["be", "quiet"] | ["shut", "up"] | ["hush"] | ["enough"] | ["thats", "enough"]
            | ["stop", "talking"] | ["silence"]
    )
}

/// A spoken answer to an approval prompt: `Some(true)` for yes.
pub fn spoken_answer(text: &str) -> Option<bool> {
    let w: Vec<String> = words(text).into_iter().map(|w| w.0).collect();
    let w: Vec<&str> = w.iter().map(String::as_str).collect();
    match w.as_slice() {
        ["yes"] | ["yeah"] | ["yep"] | ["yes", "please"] | ["approve"] | ["approved"] | ["approve", "it"]
        | ["do", "it"] | ["go", "ahead"] | ["sure"] => Some(true),
        ["no"] | ["nope"] | ["no", "thanks"] | ["deny"] | ["decline"] | ["dont"] | ["do", "not"] | ["cancel"] => {
            Some(false)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::record::tests::pcm;
    use super::*;

    #[test]
    fn hears_the_wake_phrase_as_transcribers_spell_it() {
        let m = |t: &str| match_wake(t, "orion", &[]);
        assert_eq!(m("Orion, what's using the GPU?").as_deref(), Some("what's using the GPU?"));
        assert_eq!(m("Hey Orion. Restart Ollama.").as_deref(), Some("Restart Ollama."));
        assert_eq!(m("OK, O'Ryan, how full is my disk?").as_deref(), Some("how full is my disk?"));
        assert_eq!(m("Orien what time is it").as_deref(), Some("what time is it"));
        assert_eq!(m("O Ryan, hello").as_deref(), Some("hello"));
        assert_eq!(m("Oreon.").as_deref(), Some(""));
        assert_eq!(m("orion").as_deref(), Some(""));
        // Only at the start, and not just anything that rhymes.
        assert_eq!(m("I watched a documentary about Orion"), None);
        assert_eq!(m("Thank you."), None);
        assert_eq!(m("Origin of the species"), None);
        assert_eq!(m("Brian, come here"), None);
        assert_eq!(m("Hey, can you open the door, Orion"), None);

        let multi = |t: &str| match_wake(t, "hey computer", &["hey compute".into()]);
        assert_eq!(multi("Hey computer, lights on").as_deref(), Some("lights on"));
        assert_eq!(multi("hey, computa: lights on").as_deref(), Some("lights on"));
        assert_eq!(multi("computer lights on"), None);

        assert_eq!(spoken_answer("Yes, please."), Some(true));
        assert_eq!(spoken_answer("Don't!"), Some(false));
        assert_eq!(spoken_answer("yes and also delete the logs"), None);
        assert!(is_stop_request("Stop."));
        assert!(is_stop_request("Never mind!"));
        assert!(!is_stop_request("stop the nginx service"));
    }

    fn samples(parts: &[(bool, u64)]) -> Vec<i16> {
        pcm(parts, 16_000).chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect()
    }

    #[test]
    fn cuts_utterances_on_pauses() {
        let mut seg = Segmenter::new(16_000, 600, 8000);
        let audio = samples(&[(false, 500), (true, 700), (false, 900), (true, 100), (false, 900), (true, 1200), (false, 800)]);
        let mut out = Vec::new();
        for part in audio.chunks(1000) {
            out.extend(seg.push(part));
        }
        // Two utterances; the 100 ms click in between is not one.
        assert_eq!(out.len(), 2, "{:?}", out.iter().map(Vec::len).collect::<Vec<_>>());
        // Each keeps the pre-roll and the trailing pause: 300 + speech + 600 ms.
        assert_eq!(out[0].len(), 16 * (300 + 700 + 600));
        assert_eq!(out[1].len(), 16 * (300 + 1200 + 600));
        assert!(!seg.in_speech());

        // Long speech is cut at the limit.
        let mut seg = Segmenter::new(16_000, 600, 2000);
        let out = seg.push(&samples(&[(false, 400), (true, 5000)]));
        assert_eq!(out.len(), 2);
        assert!(seg.in_speech());
    }

    /// A transcriber that answers with the contents of a file, one line per
    /// call, so a test can script what was "said".
    fn scripted(lines: &[&str]) -> (Transcriber, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("said");
        std::fs::write(&script, lines.join("\n") + "\n").unwrap();
        let n = dir.path().join("n");
        std::fs::write(&n, "0").unwrap();
        let cmd = format!(
            "i=$(cat {n}); echo $((i+1)) > {n}; sed -n \"$((i+1))p\" {s}",
            n = n.display(),
            s = script.display()
        );
        (Transcriber::Command(cmd), dir)
    }

    fn wait(l: &WakeListener) -> WakeEvent {
        let start = Instant::now();
        loop {
            if let Some(e) = l.try_event() {
                return e;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "no wake event");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn listens_hands_free() {
        // Chatter, then "Orion, …" in one go, then a bare "Orion" and the request after it.
        let audio = pcm(
            &[(false, 400), (true, 800), (false, 900), (true, 900), (false, 900), (true, 500), (false, 900), (true, 900), (false, 2000)],
            16_000,
        );
        let (transcriber, _dir) = scripted(&["Nice weather today.", "Orion, what's using the GPU?", "Orion.", "Restart Ollama."]);
        let (recorder, _f) = super::super::record::tests::recorder_for(&audio, 1200);
        let config = VoiceConfig { wake_chime: false, silence_ms: 700, ..Default::default() };
        let l = WakeListener::start(&config, recorder, transcriber, None).unwrap();
        assert_eq!(wait(&l), WakeEvent::Request("what's using the GPU?".into()));
        assert_eq!(wait(&l), WakeEvent::Woke);
        assert_eq!(wait(&l), WakeEvent::Request("Restart Ollama.".into()));
        assert_eq!(l.state(), WakeState::Waiting);
        l.set_paused(true);
        assert_eq!(l.state(), WakeState::Paused);
    }

    #[test]
    fn a_detector_command_can_do_the_listening() {
        let audio = pcm(&[(false, 400), (true, 900), (false, 1500)], 16_000);
        let (transcriber, _dir) = scripted(&["Restart Ollama."]);
        let (recorder, _f) = super::super::record::tests::recorder_for(&audio, 700);
        let config = VoiceConfig {
            wake_chime: false,
            wake_command: "echo detected; sleep 30".into(),
            silence_ms: 700,
            ..Default::default()
        };
        let l = WakeListener::start(&config, recorder, transcriber, None).unwrap();
        assert_eq!(wait(&l), WakeEvent::Woke);
        assert_eq!(wait(&l), WakeEvent::Request("Restart Ollama.".into()));
    }

    #[test]
    fn only_the_enrolled_voice_gets_in() {
        use super::super::speaker::tests::voice;
        let me = |text, secs| voice(120.0, 1.0, text, secs);
        let enroll: Vec<Vec<i16>> = (1..=6).map(|t| me(t, 3.0)).collect();
        let vp = super::super::speaker::Voiceprint::enroll(&enroll, 16_000, None).unwrap();
        let quiet = |ms: usize| vec![0i16; 16 * ms];
        // You ask; someone else talks; you say "Orion" and they make the request.
        let audio: Vec<i16> = [quiet(500), me(21, 2.0), quiet(900), voice(210.0, 1.18, 22, 2.0), quiet(900), me(23, 0.6), quiet(900), voice(210.0, 1.18, 24, 1.5), quiet(2500)].concat();
        let pcm: Vec<u8> = audio.iter().flat_map(|s| s.to_le_bytes()).collect();
        let (transcriber, dir) = scripted(&["Orion, what's using the GPU?", "Orion.", "Install everything."]);
        let (recorder, _f) = super::super::record::tests::recorder_for(&pcm, 700);
        let config = VoiceConfig { wake_chime: false, silence_ms: 700, ..Default::default() };
        let l = WakeListener::start(&config, recorder, transcriber, Some(Judge::new(vp, None, 1.2, 0.8))).unwrap();
        assert!(l.describe().ends_with("only your voice"));
        assert_eq!(wait(&l), WakeEvent::Request("what's using the GPU?".into()));
        assert_eq!(wait(&l), WakeEvent::Woke);
        assert_eq!(wait(&l), WakeEvent::NotYou);
        // The other voice was never transcribed: two calls, both for you.
        assert_eq!(std::fs::read_to_string(dir.path().join("n")).unwrap().trim(), "2");
        assert_eq!(l.ignored(), 2);
    }

    #[test]
    fn refuses_to_stream_the_room_to_a_remote_transcriber() {
        let remote = VoiceConfig { transcribe_url: "https://api.example.com/v1".into(), ..Default::default() };
        let t = Transcriber::from_config(&remote).unwrap();
        let (recorder, _f) = super::super::record::tests::recorder_for(&[], 700);
        let err = WakeListener::start(&remote, recorder, t, None).err().unwrap();
        assert!(err.to_string().contains("api.example.com"), "{err}");
    }
}
