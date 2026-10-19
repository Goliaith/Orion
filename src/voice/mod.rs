//! Voice: talk to Orion and hear it answer.
//!
//! Local-first: the microphone is read through whatever this machine has
//! (PulseAudio/PipeWire, ALSA, sox, ffmpeg), speech is transcribed by a
//! local server or command, and replies are spoken by a local
//! text-to-speech command or server. Nothing leaves the machine unless a
//! URL here points elsewhere.

pub mod record;
pub mod speak;
pub mod transcribe;

use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::Deserialize;

pub use record::{Recorder, Recording};
pub use speak::{Sentences, Speaker};
pub use transcribe::Transcriber;

/// `[voice]` in orion.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VoiceConfig {
    pub enabled: bool,
    /// A command writing raw 16-bit little-endian mono PCM at `sample_rate`
    /// to stdout. Empty: the first of parecord, arecord, sox, ffmpeg found.
    pub record_command: String,
    pub sample_rate: u32,
    /// Stop listening after this much silence following speech; 0 means
    /// only Ctrl-T stops.
    pub silence_ms: u64,
    pub max_seconds: u64,
    /// OpenAI-compatible base URL serving `/audio/transcriptions`
    /// (speaches / faster-whisper-server, LocalAI, whisper.cpp's server).
    pub transcribe_url: String,
    pub transcribe_model: String,
    /// Instead of the URL: a shell command printing the transcript, with
    /// `{wav}` replaced by the recording's path.
    pub transcribe_command: String,
    /// Language hint such as "en"; empty lets the model detect it.
    pub language: String,
    pub transcribe_timeout_secs: u64,
    /// Send a transcript right away instead of leaving it to edit.
    pub send_after_transcribe: bool,
    /// Speak replies aloud from the start (`/voice speak on|off` toggles).
    pub speak_replies: bool,
    /// A shell command that speaks the text on its stdin. Empty: espeak-ng,
    /// espeak or spd-say, whichever is installed.
    pub speak_command: String,
    /// Instead of the command: OpenAI-compatible base URL serving
    /// `/audio/speech` (Kokoro-FastAPI, speaches, LocalAI).
    pub speak_url: String,
    pub speak_model: String,
    pub speak_voice: String,
    /// Plays a WAV file from `speak_url`, with `{wav}` as its path. Empty:
    /// pw-play, paplay, aplay or ffplay.
    pub play_command: String,
    /// Environment variable with a bearer token, for servers that want one.
    pub api_key_env: String,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            record_command: String::new(),
            sample_rate: 16_000,
            silence_ms: 1200,
            max_seconds: 60,
            transcribe_url: "http://127.0.0.1:8000/v1".into(),
            transcribe_model: "Systran/faster-whisper-small".into(),
            transcribe_command: String::new(),
            language: String::new(),
            transcribe_timeout_secs: 60,
            send_after_transcribe: false,
            speak_replies: false,
            speak_command: String::new(),
            speak_url: String::new(),
            speak_model: "kokoro".into(),
            speak_voice: "af_heart".into(),
            play_command: String::new(),
            api_key_env: String::new(),
        }
    }
}

/// Everything an interface needs for voice, with each part's setup error
/// kept for display instead of failing the chat.
pub struct VoiceIo {
    pub config: VoiceConfig,
    pub recorder: Result<Recorder, String>,
    pub transcriber: Result<Transcriber, String>,
    pub speaker: Result<Speaker, String>,
    pub speak_replies: bool,
    sentences: Sentences,
}

impl VoiceIo {
    pub fn new(config: &VoiceConfig) -> Self {
        let err = |e: anyhow::Error| format!("{e:#}");
        Self {
            recorder: Recorder::detect(config).map_err(err),
            transcriber: Transcriber::from_config(config).map_err(err),
            speaker: Speaker::from_config(config).map_err(err),
            speak_replies: config.speak_replies,
            config: config.clone(),
            sentences: Sentences::default(),
        }
    }

    /// Feed streamed reply text; complete sentences are spoken if replies
    /// are being spoken.
    pub fn reply_text(&mut self, text: &str) {
        let done = self.sentences.push(text);
        if let (true, Ok(s)) = (self.speak_replies, &self.speaker) {
            done.into_iter().for_each(|t| s.say(&t));
        }
    }

    /// The reply ended: speak what is left.
    pub fn reply_done(&mut self) {
        let rest = self.sentences.finish();
        if let (true, Ok(s), Some(t)) = (self.speak_replies, &self.speaker, rest) {
            s.say(&t);
        }
    }

    /// Stop talking (Ctrl-C, or the user started a new message).
    pub fn hush(&mut self) -> bool {
        self.sentences = Sentences::default();
        match &self.speaker {
            Ok(s) if s.is_speaking() => {
                s.stop();
                true
            }
            _ => false,
        }
    }

    /// `/voice …`: show the setup, or `speak on|off`, or `test`.
    pub fn command(&mut self, arg: &str) -> String {
        match arg.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["speak", "on"] | ["on"] => {
                self.speak_replies = true;
                match &self.speaker {
                    Ok(_) => "  replies will be spoken".into(),
                    Err(e) => format!("  replies will be spoken once speech works: {e}"),
                }
            }
            ["speak", "off"] | ["off"] => {
                self.speak_replies = false;
                self.hush();
                "  replies are silent".into()
            }
            ["test"] => match &self.speaker {
                Ok(s) => {
                    s.say("Orion's voice is working.");
                    "  speaking a test sentence".into()
                }
                Err(e) => format!("  speech is not set up: {e}"),
            },
            _ => {
                let part = |r: Result<String, &String>| match r {
                    Ok(d) => d,
                    Err(e) => format!("unavailable: {e}"),
                };
                [
                    format!("  microphone   {}", part(self.recorder.as_ref().map(Recorder::describe))),
                    format!("  transcribe   {}", part(self.transcriber.as_ref().map(Transcriber::describe))),
                    format!("  speak        {}", part(self.speaker.as_ref().map(Speaker::describe))),
                    format!(
                        "  replies      {}",
                        if self.speak_replies { "spoken (/voice speak off)" } else { "silent (/voice speak on)" }
                    ),
                    "  Ctrl-T starts listening; pause to finish (or Ctrl-T full screen, Enter line by line).".into(),
                ]
                .join("\n")
            }
        }
    }
}

/// The first of `names` on PATH.
fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?.to_str()?.split(':').map(|d| PathBuf::from(d).join(name)).find(|p| p.is_file())
}

/// Quote for `sh`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A `sh -c` command in its own process group, so it can be stopped whole.
fn shell(command: &str) -> Command {
    use std::os::unix::process::CommandExt;
    let mut c = Command::new("sh");
    c.arg("-c").arg(command).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
    c
}

fn kill_group(pid: u32, signal: libc::c_int) {
    // SAFETY: kill(2) has no memory-safety preconditions.
    unsafe {
        libc::kill(-(pid as libc::pid_t), signal);
    }
}

/// A temporary file path that is removed when dropped.
struct TempPath(PathBuf);

impl TempPath {
    fn new(ext: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("orion-{}-{n}.{ext}", std::process::id())))
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A WAV file from 16-bit mono samples.
pub fn wav(samples: &[i16], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header() {
        let w = wav(&[0, 1, -1], 16_000);
        assert_eq!(w.len(), 44 + 6);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(w[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()), 6);
        assert_eq!(&w[44..], &[0, 0, 1, 0, 0xff, 0xff]);
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("/tmp/it's.wav"), r"'/tmp/it'\''s.wav'");
    }
}
