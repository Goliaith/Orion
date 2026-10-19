//! Text to speech: a speaker thread that says queued sentences one after
//! another through a command (text on stdin) or an OpenAI-compatible
//! speech server plus a player, and can be stopped mid-sentence.

use std::io::Write;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::json;

use super::{kill_group, shell, shell_quote, which, TempPath, VoiceConfig};
use crate::chat::http::{non_empty_env, Http};

enum Backend {
    /// Speaks its stdin.
    Command(String),
    /// Synthesizes WAV, then `player` (with `{wav}`) plays it.
    Http { http: Http, model: String, voice: String, player: String },
}

#[derive(Default)]
struct State {
    /// Bumped by `stop`; queued sentences from before it are dropped.
    generation: AtomicU64,
    speaking: AtomicBool,
    /// Process group of the command speaking right now.
    current: Mutex<Option<u32>>,
}

pub struct Speaker {
    tx: Sender<(u64, String)>,
    state: Arc<State>,
    description: String,
}

impl Speaker {
    pub fn from_config(config: &VoiceConfig) -> Result<Self> {
        let backend = if !config.speak_url.is_empty() {
            let mut headers = Vec::new();
            if !config.api_key_env.is_empty() {
                let key = non_empty_env(&config.api_key_env).with_context(|| format!("{} is not set", config.api_key_env))?;
                headers.push(("authorization".to_string(), format!("Bearer {key}")));
            }
            let player = if config.play_command.is_empty() {
                [("pw-play", "pw-play {wav}"), ("paplay", "paplay {wav}"), ("aplay", "aplay -q {wav}"), ("ffplay", "ffplay -nodisp -autoexit -loglevel quiet {wav}")]
                    .into_iter()
                    .find(|(bin, _)| which(bin).is_some())
                    .map(|(_, c)| c.to_string())
                    .context("no audio player found: install pipewire, pulseaudio-utils, alsa-utils or ffmpeg, or set voice.play_command")?
            } else {
                config.play_command.clone()
            };
            Backend::Http {
                http: Http::new(&config.speak_url, headers),
                model: config.speak_model.clone(),
                voice: config.speak_voice.clone(),
                player,
            }
        } else if !config.speak_command.is_empty() {
            Backend::Command(config.speak_command.clone())
        } else {
            [("espeak-ng", "espeak-ng --stdin"), ("espeak", "espeak --stdin"), ("spd-say", "spd-say -w -e")]
                .into_iter()
                .find(|(bin, _)| which(bin).is_some())
                .map(|(_, c)| Backend::Command(c.to_string()))
                .context("no speech command found: install espeak-ng or speech-dispatcher, or set voice.speak_command (piper) or voice.speak_url")?
        };
        Ok(Self::spawn(backend))
    }

    fn spawn(backend: Backend) -> Self {
        let description = match &backend {
            Backend::Command(c) => c.split_whitespace().next().unwrap_or_default().to_string(),
            Backend::Http { http, model, voice, .. } => format!("{model} ({voice}) at {}", http.base()),
        };
        let (tx, rx) = mpsc::channel::<(u64, String)>();
        let state = Arc::new(State::default());
        let st = state.clone();
        thread::spawn(move || {
            for (generation, text) in rx {
                if generation != st.generation.load(Ordering::SeqCst) {
                    continue;
                }
                st.speaking.store(true, Ordering::SeqCst);
                if let Err(e) = say_now(&backend, &text, &st, generation) {
                    tracing::warn!("speech failed: {e:#}");
                }
                st.speaking.store(false, Ordering::SeqCst);
            }
        });
        Self { tx, state, description }
    }

    pub fn describe(&self) -> String {
        self.description.clone()
    }

    /// Queue a sentence.
    pub fn say(&self, text: &str) {
        let text = text.trim();
        if !text.is_empty() {
            let _ = self.tx.send((self.state.generation.load(Ordering::SeqCst), text.to_string()));
        }
    }

    /// Stop the current sentence and drop the queue.
    pub fn stop(&self) {
        self.state.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(pid) = *self.state.current.lock().unwrap_or_else(|e| e.into_inner()) {
            kill_group(pid, libc::SIGKILL);
        }
    }

    pub fn is_speaking(&self) -> bool {
        self.state.speaking.load(Ordering::SeqCst)
    }
}

impl Drop for Speaker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Run one command to completion, unless `stop` is called.
fn run(command: &str, stdin: Option<&str>, st: &State, generation: u64) -> Result<()> {
    let mut cmd = shell(command);
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd.spawn().with_context(|| format!("running {command}"))?;
    *st.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(child.id());
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let _ = pipe.write_all(text.as_bytes());
    }
    // Poll so a stop between spawn and recording the pid still lands.
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if generation != st.generation.load(Ordering::SeqCst) {
            kill_group(child.id(), libc::SIGKILL);
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(20));
    };
    *st.current.lock().unwrap_or_else(|e| e.into_inner()) = None;
    if !status.success() && generation == st.generation.load(Ordering::SeqCst) {
        bail!("{command} exited with {status}");
    }
    Ok(())
}

fn say_now(backend: &Backend, text: &str, st: &State, generation: u64) -> Result<()> {
    match backend {
        Backend::Command(c) => run(c, Some(text), st, generation),
        Backend::Http { http, model, voice, player } => {
            let body = json!({ "model": model, "voice": voice, "input": text, "response_format": "wav" });
            let audio = http.post_bytes("/audio/speech", "application/json", serde_json::to_vec(&body)?, Duration::from_secs(60))?;
            if generation != st.generation.load(Ordering::SeqCst) {
                return Ok(());
            }
            let file = TempPath::new("wav");
            std::fs::write(&file.0, audio)?;
            run(&player.replace("{wav}", &shell_quote(&file.0.to_string_lossy())), None, st, generation)
        }
    }
}

/// Turns a streamed reply into speakable sentences: split at sentence ends
/// and line breaks, without code blocks and Markdown symbols.
#[derive(Default)]
pub struct Sentences {
    buf: String,
    in_code: bool,
}

impl Sentences {
    /// Add streamed text; returns the sentences it completed.
    pub fn push(&mut self, text: &str) -> Vec<String> {
        self.buf.push_str(text);
        let mut out = Vec::new();
        while let Some(end) = self.boundary() {
            let piece: String = self.buf.drain(..end).collect();
            self.take(piece, &mut out);
        }
        out
    }

    /// The rest, once the reply is over.
    pub fn finish(&mut self) -> Option<String> {
        let mut out = Vec::new();
        let rest = std::mem::take(&mut self.buf);
        self.take(rest, &mut out);
        self.in_code = false;
        (!out.is_empty()).then(|| out.join(" "))
    }

    /// End (exclusive) of the first complete line or sentence in `buf`.
    fn boundary(&self) -> Option<usize> {
        let b = self.buf.as_bytes();
        for (i, &c) in b.iter().enumerate() {
            if c == b'\n' {
                return Some(i + 1);
            }
            // ". " ends a sentence, unless it's a number like "1. " or "3.5".
            if matches!(c, b'.' | b'!' | b'?' | b':' | b';') && b.get(i + 1).is_some_and(|n| *n == b' ') {
                let numbered = c == b'.' && i > 0 && b[i - 1].is_ascii_digit() && (i < 2 || !b[i - 2].is_ascii_alphanumeric());
                if !numbered && i >= 12 {
                    return Some(i + 2);
                }
            }
        }
        None
    }

    fn take(&mut self, piece: String, out: &mut Vec<String>) {
        for line in piece.split('\n') {
            if line.trim_start().starts_with("```") {
                if !self.in_code {
                    out.push("(code shown on screen)".into());
                }
                self.in_code = !self.in_code;
                continue;
            }
            if self.in_code {
                continue;
            }
            let spoken = speakable(line);
            if !spoken.is_empty() {
                out.push(spoken);
            }
        }
    }
}

/// Markdown and symbols removed, ready for a speech engine.
fn speakable(line: &str) -> String {
    let t = line.trim();
    let t = t.trim_start_matches('#').trim_start_matches(['-', '*', '>', '•']).trim();
    let t: String = t.chars().filter(|c| !matches!(c, '*' | '`' | '_' | '|')).collect();
    let t = t.replace("→", " to ").replace("~/", "home folder ").replace(" & ", " and ");
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn sentences_from_a_stream() {
        let mut s = Sentences::default();
        let mut said = Vec::new();
        for chunk in ["Your disk is 28% ", "full. The biggest ", "folder is ~/Videos!", " Want me to", "\n\n```sh\ndu -sh *\n```\n", "**1. Clean** the cache."] {
            said.extend(s.push(chunk));
        }
        said.extend(s.finish());
        assert_eq!(
            said,
            [
                "Your disk is 28% full.",
                "The biggest folder is home folder Videos!",
                "Want me to",
                "(code shown on screen)",
                "1. Clean the cache.",
            ]
        );
    }

    #[test]
    fn speaks_in_order_and_stops_mid_sentence() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("said.txt");
        let backend = Backend::Command(format!("cat >> {}; echo >> {}", log.display(), log.display()));
        let speaker = Speaker::spawn(backend);
        speaker.say("one");
        speaker.say("two");
        let start = Instant::now();
        while std::fs::read_to_string(&log).unwrap_or_default().lines().count() < 2 {
            assert!(start.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "one\ntwo\n");

        let slow = Speaker::spawn(Backend::Command("sleep 30".into()));
        slow.say("a long sentence");
        slow.say("never said");
        while !slow.is_speaking() {
            thread::sleep(Duration::from_millis(10));
        }
        let start = Instant::now();
        slow.stop();
        while slow.is_speaking() {
            assert!(start.elapsed() < Duration::from_secs(2), "stop did not end the sentence");
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(100));
        assert!(!slow.is_speaking(), "the queue was dropped");
    }
}
