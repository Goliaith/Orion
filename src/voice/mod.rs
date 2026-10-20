//! Voice: talk to Orion and hear it answer.
//!
//! Local-first: the microphone is read through whatever this machine has
//! (PulseAudio/PipeWire, ALSA, sox, ffmpeg), speech is transcribed by a
//! local server or command, and replies are spoken by a local
//! text-to-speech command or server. Nothing leaves the machine unless a
//! URL here points elsewhere.

pub mod enroll;
pub mod record;
pub mod speak;
pub mod speaker;
pub mod transcribe;
pub mod wake;

use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::Deserialize;

pub use record::{Recorder, Recording};
pub use speak::{Sentences, Speaker};
pub use transcribe::Transcriber;
pub use wake::{is_stop_request, spoken_answer, WakeEvent, WakeListener, WakeState};

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
    /// Listen for the wake phrase from the start (`/voice wake on|off`,
    /// Alt-W). Only in the full-screen chat and the shell.
    pub wake: bool,
    /// Said at the start of a request: "Orion, …", "hey Orion, …".
    pub wake_phrase: String,
    /// Other spellings the transcriber writes for the phrase.
    pub wake_aliases: Vec<String>,
    /// A wake-word detector to use instead of transcribing: a shell command
    /// that prints a line each time it hears the word.
    pub wake_command: String,
    /// Play a chime when woken without a request.
    pub wake_chime: bool,
    /// How long to wait for the request after a bare wake phrase.
    pub wake_timeout_secs: u64,
    /// Allow wake listening with a transcriber on another machine, which
    /// then hears everything said near the microphone.
    pub wake_allow_remote: bool,
    /// Let "Orion, yes" approve a tool call. "No" always declines.
    pub wake_approvals: bool,
    /// Only the enrolled voice can wake Orion: `auto` once a voice is
    /// enrolled (`orion voice enroll`), `on` (refuse to listen without
    /// one), or `off`.
    pub verify_speaker: Verify,
    /// How close to the enrolled voice: the distortion ratio allowed, where
    /// 1.0 is typical for the enrolled speaker. Lower is stricter.
    pub verify_threshold: f32,
    /// `orion listen`: start a new conversation when the last request is
    /// this many minutes old (0: one conversation for good).
    pub listen_new_after_mins: u64,
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
            wake: false,
            wake_phrase: "orion".into(),
            wake_aliases: Vec::new(),
            wake_command: String::new(),
            wake_chime: true,
            wake_timeout_secs: 8,
            wake_allow_remote: false,
            wake_approvals: false,
            listen_new_after_mins: 10,
            verify_speaker: Verify::Auto,
            verify_threshold: 1.2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verify {
    #[default]
    Auto,
    On,
    Off,
}

impl VoiceConfig {
    /// The voiceprint wake listening should hold to, if any.
    pub fn voiceprint(&self) -> Result<Option<speaker::Voiceprint>, String> {
        if self.verify_speaker == Verify::Off {
            return Ok(None);
        }
        let path = speaker::Voiceprint::default_path().ok_or("no state directory for the voiceprint (is HOME set?)")?;
        if !path.exists() {
            return match self.verify_speaker {
                Verify::On => Err("speaker verification is on, but no voice is enrolled: run `orion voice enroll`".into()),
                _ => Ok(None),
            };
        }
        speaker::Voiceprint::load(&path).map(Some).map_err(|e| format!("{e:#}"))
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
    /// Listening for the wake phrase, when on.
    wake: Option<WakeListener>,
    /// Whether this interface handles wake events (the line-by-line chat
    /// can't take input while it waits at its prompt).
    wake_supported: bool,
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
            wake: None,
            wake_supported: false,
        }
    }

    /// For interfaces that handle wake events: allow wake listening, and
    /// start it if the policy says so. An error explains why it didn't start.
    pub fn support_wake(&mut self) -> Result<(), String> {
        self.wake_supported = true;
        if self.config.enabled && self.config.wake {
            self.wake_on().map(drop)
        } else {
            Ok(())
        }
    }

    /// Start listening for the wake phrase; returns what to say.
    pub fn wake_on(&mut self) -> Result<String, String> {
        if !self.wake_supported {
            return Err("wake listening works in the full-screen chat and in the shell".into());
        }
        if self.wake.as_ref().is_some_and(|w| w.state() != WakeState::Stopped) {
            return Ok(self.wake.as_ref().map(|w| w.describe().to_string()).unwrap_or_default());
        }
        let recorder = self.recorder.clone()?;
        let transcriber = self.transcriber.clone().map_err(|e| format!("can't transcribe: {e}"))?;
        let voiceprint = self.config.voiceprint()?;
        let w = WakeListener::start(&self.config, recorder, transcriber, voiceprint).map_err(|e| format!("{e:#}"))?;
        let what = w.describe().to_string();
        self.wake = Some(w);
        Ok(what)
    }

    pub fn wake_off(&mut self) {
        self.wake = None;
    }

    pub fn wake(&self) -> Option<&WakeListener> {
        self.wake.as_ref()
    }

    /// The next wake event, if any. A listener that failed is turned off.
    pub fn wake_event(&mut self) -> Option<WakeEvent> {
        let ev = self.wake.as_ref()?.try_event();
        if matches!(ev, Some(WakeEvent::Failed(_))) {
            self.wake = None;
        }
        ev
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
            ["wake", "on"] => match self.wake_on() {
                Ok(what) => format!("  listening for the wake phrase: {what}"),
                Err(e) => format!("  can't listen for the wake phrase: {e}"),
            },
            ["wake", "off"] => {
                self.wake_off();
                "  stopped listening for the wake phrase".into()
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
                    format!(
                        "  speaker      {}",
                        match (self.config.voiceprint(), &self.wake) {
                            (Err(e), _) => format!("unavailable: {e}"),
                            (Ok(None), _) => "anyone (orion voice enroll: only you)".into(),
                            (Ok(Some(v)), Some(w)) => format!("only your voice ({} phrases enrolled); {} others ignored", v.phrases, w.ignored()),
                            (Ok(Some(v)), None) => format!("only your voice ({} phrases enrolled)", v.phrases),
                        }
                    ),
                    format!(
                        "  wake word    {}",
                        match &self.wake {
                            Some(w) => format!("{} (/voice wake off)", w.describe()),
                            None if self.wake_supported => "off (/voice wake on, or Alt-W)".into(),
                            None => "full-screen chat and shell only".into(),
                        }
                    ),
                    "  Ctrl-T starts listening; pause to finish (or Ctrl-T full screen, Enter line by line).".into(),
                ]
                .join("\n")
            }
        }
    }
}

/// The command that plays a WAV file (`{wav}`): `voice.play_command`, or
/// the first of pw-play, paplay, aplay, ffplay that is installed.
fn player(config: &VoiceConfig) -> Option<String> {
    if !config.play_command.is_empty() {
        return Some(config.play_command.clone());
    }
    [("pw-play", "pw-play {wav}"), ("paplay", "paplay {wav}"), ("aplay", "aplay -q {wav}"), ("ffplay", "ffplay -nodisp -autoexit -loglevel quiet {wav}")]
        .into_iter()
        .find(|(bin, _)| which(bin).is_some())
        .map(|(_, c)| c.to_string())
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

/// Have the kernel kill the child when the thread that started it ends,
/// even if Orion itself is killed and can't clean up.
fn die_with_parent(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: prctl is async-signal-safe and touches no memory of ours.
    unsafe {
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
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
