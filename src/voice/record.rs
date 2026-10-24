//! Listening: run a recorder that writes raw PCM to stdout, measure the
//! level as it arrives, and stop on a pause after speech.

use std::io::Read;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use super::tuning::{Ended, Take, VoiceLog};
use super::{kill_group, shell, which, VoiceConfig};

/// 100 ms of audio per measurement.
pub(crate) const CHUNK_MS: u64 = 100;
/// Speech must last this long before a pause can end the recording, so a
/// click or a cough does not.
pub(crate) const MIN_SPEECH_MS: u64 = 300;

/// What counts as speech: louder than the room's noise by `margin`, and
/// never quieter than `level`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Sensitivity {
    pub margin: f32,
    pub level: f32,
}

impl Default for Sensitivity {
    fn default() -> Self {
        Self { margin: 3.0, level: 300.0 }
    }
}

impl Sensitivity {
    pub fn from_config(config: &VoiceConfig) -> Self {
        Self { margin: config.noise_margin.max(1.0), level: config.speech_level.max(0.0) }
    }

    /// Louder than this counts as speech.
    pub fn threshold(&self, noise_floor: Option<f32>) -> f32 {
        (noise_floor.unwrap_or(0.0) * self.margin).max(self.level)
    }
}

#[derive(Clone)]
pub struct Recorder {
    command: String,
    rate: u32,
    silence_ms: u64,
    max: Duration,
    sensitivity: Sensitivity,
}

impl Recorder {
    pub fn detect(config: &VoiceConfig) -> Result<Self> {
        let rate = config.sample_rate;
        let command = if !config.record_command.is_empty() {
            config.record_command.clone()
        } else {
            let candidates = [
                ("parecord", format!("parecord --raw --rate={rate} --channels=1 --format=s16le")),
                ("arecord", format!("arecord -q -t raw -f S16_LE -r {rate} -c 1")),
                ("sox", format!("sox -q -d -t raw -r {rate} -c 1 -b 16 -e signed-integer -L -")),
                ("ffmpeg", format!("ffmpeg -loglevel quiet -f pulse -i default -ac 1 -ar {rate} -f s16le -")),
            ];
            candidates
                .into_iter()
                .find(|(bin, _)| which(bin).is_some())
                .map(|(_, cmd)| cmd)
                .context("no recorder found: install pulseaudio-utils (parecord), alsa-utils (arecord), sox or ffmpeg, or set voice.record_command")?
        };
        Ok(Self {
            command,
            rate,
            silence_ms: config.silence_ms,
            max: Duration::from_secs(config.max_seconds.max(1)),
            sensitivity: Sensitivity::from_config(config),
        })
    }

    pub fn describe(&self) -> String {
        self.command.split_whitespace().next().unwrap_or_default().to_string()
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub(crate) fn sensitivity(&self) -> Sensitivity {
        self.sensitivity
    }

    /// The recorder process, writing raw PCM to its stdout.
    pub fn spawn(&self) -> Result<Child> {
        let mut cmd = shell(&self.command);
        cmd.stdout(Stdio::piped());
        cmd.spawn().with_context(|| format!("starting {}", self.describe()))
    }

    /// Like [`spawn`](Self::spawn), for a recorder that should never outlive
    /// the calling thread (an always-open microphone).
    pub(crate) fn spawn_tied(&self) -> Result<Child> {
        let mut cmd = shell(&self.command);
        cmd.stdout(Stdio::piped());
        super::die_with_parent(&mut cmd);
        cmd.spawn().with_context(|| format!("starting {}", self.describe()))
    }

    pub fn start(&self) -> Result<Recording> {
        let mut child = self.spawn()?;
        let mut stdout = child.stdout.take().context("recorder has no stdout")?;
        let shared = Arc::new(Mutex::new(Capture { sensitivity: self.sensitivity, ..Default::default() }));
        let chunk_bytes = (self.rate as u64 * 2 * CHUNK_MS / 1000) as usize;
        let reader = {
            let shared = shared.clone();
            thread::spawn(move || {
                let mut buf = vec![0u8; chunk_bytes];
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
                    let samples: Vec<i16> = bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
                    shared.lock().unwrap_or_else(|e| e.into_inner()).add(&samples, chunk_bytes / 2);
                }
                shared.lock().unwrap_or_else(|e| e.into_inner()).ended = true;
            })
        };
        Ok(Recording {
            child: Some(child),
            reader: Some(reader),
            shared,
            started: Instant::now(),
            rate: self.rate,
            silence_ms: self.silence_ms,
            max: self.max,
        })
    }
}

/// Root-mean-square amplitude of a chunk.
pub(crate) fn rms(chunk: &[i16]) -> f32 {
    (chunk.iter().map(|&s| (s as f32).powi(2)).sum::<f32>() / chunk.len().max(1) as f32).sqrt()
}

/// A meter from 0 (-60 dBFS or quieter) to 1 (full scale).
pub(crate) fn meter(rms: f32) -> f32 {
    let db = 20.0 * (rms.max(1.0) / 32768.0).log10();
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

#[derive(Default)]
struct Capture {
    samples: Vec<i16>,
    /// Samples not yet measured.
    pending: Vec<i16>,
    level: f32,
    noise_floor: Option<f32>,
    sensitivity: Sensitivity,
    chunks: u32,
    speech_ms: u64,
    silent_ms: u64,
    ended: bool,
}

impl Capture {
    fn add(&mut self, samples: &[i16], chunk: usize) {
        self.samples.extend_from_slice(samples);
        self.pending.extend_from_slice(samples);
        while self.pending.len() >= chunk {
            let part: Vec<i16> = self.pending.drain(..chunk).collect();
            self.measure(&part);
        }
    }

    fn measure(&mut self, chunk: &[i16]) {
        let rms = rms(chunk);
        self.level = meter(rms);
        // The quietest of the first few chunks is the room's noise floor.
        self.chunks += 1;
        if self.chunks <= 3 {
            self.noise_floor = Some(self.noise_floor.map_or(rms, |f| f.min(rms)));
        }
        if rms > self.sensitivity.threshold(self.noise_floor) {
            self.speech_ms += CHUNK_MS;
            self.silent_ms = 0;
        } else {
            self.silent_ms += CHUNK_MS;
        }
    }
}

/// A recording in progress.
pub struct Recording {
    child: Option<Child>,
    reader: Option<JoinHandle<()>>,
    shared: Arc<Mutex<Capture>>,
    started: Instant,
    rate: u32,
    silence_ms: u64,
    max: Duration,
}

impl Recording {
    fn with<T>(&self, f: impl FnOnce(&Capture) -> T) -> T {
        f(&self.shared.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Input level from 0 to 1, for a meter.
    pub fn level(&self) -> f32 {
        self.with(|c| c.level)
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Whether it ran to `max_seconds` instead of ending on a pause.
    pub fn hit_limit(&self) -> bool {
        self.elapsed() >= self.max
    }

    pub fn heard_speech(&self) -> bool {
        self.with(|c| c.speech_ms >= MIN_SPEECH_MS)
    }

    /// True once the speaker paused after talking, the time limit passed,
    /// or the recorder stopped.
    pub fn should_stop(&self) -> bool {
        let paused = |c: &Capture| self.silence_ms > 0 && c.speech_ms >= MIN_SPEECH_MS && c.silent_ms >= self.silence_ms;
        self.elapsed() >= self.max || self.with(|c| c.ended || paused(c))
    }

    fn stop_recorder(&mut self) -> Option<String> {
        let mut child = self.child.take()?;
        kill_group(child.id(), libc::SIGINT);
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        kill_group(child.id(), libc::SIGKILL);
        let status = child.wait().ok();
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
        // A recorder that failed outright (no device, say) exits non-zero
        // before producing anything.
        let empty = self.with(|c| c.samples.is_empty());
        match status {
            Some(s) if empty && !s.success() && s.code().is_some() => Some(format!("the recorder exited with {s}")),
            _ => None,
        }
    }

    /// Stop and return the audio as WAV.
    pub fn finish(mut self) -> Result<Vec<u8>> {
        let ended = if self.hit_limit() {
            Ended::Limit
        } else if self.with(|c| self.silence_ms > 0 && c.silent_ms >= self.silence_ms) {
            Ended::Pause
        } else {
            Ended::Stopped
        };
        let samples = self.take()?;
        let (heard, floor, sensitivity) = self.with(|c| (c.speech_ms >= MIN_SPEECH_MS, c.noise_floor, c.sensitivity));
        if !heard {
            bail!("heard nothing that sounded like speech");
        }
        if let Some(t) = Take::measure(&samples, self.rate, floor, sensitivity, self.silence_ms, ended) {
            VoiceLog::add_take(t);
        }
        Ok(super::wav(&samples, self.rate))
    }

    /// Stop and return the samples, speech or not.
    pub(crate) fn finish_raw(mut self) -> Result<Vec<i16>> {
        self.take()
    }

    fn take(&mut self) -> Result<Vec<i16>> {
        if let Some(why) = self.stop_recorder() {
            bail!("{why}: is a microphone connected?");
        }
        let samples = self.with(|c| c.samples.clone());
        if samples.len() < (self.rate / 4) as usize {
            bail!("the recording is empty: is a microphone connected?");
        }
        Ok(samples)
    }

    pub fn cancel(mut self) {
        self.stop_recorder();
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.stop_recorder();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write;

    use super::*;

    /// Raw PCM: `ms` of a loud tone or of near-silence.
    pub fn pcm(parts: &[(bool, u64)], rate: u32) -> Vec<u8> {
        let mut out = Vec::new();
        for &(loud, ms) in parts {
            for i in 0..(rate as u64 * ms / 1000) {
                let s: i16 = if loud { ((i as f32 * 0.07).sin() * 8000.0) as i16 } else { ((i % 7) as i16) - 3 };
                out.extend_from_slice(&s.to_le_bytes());
            }
        }
        out
    }

    pub fn recorder_for(pcm: &[u8], silence_ms: u64) -> (Recorder, tempfile::NamedTempFile) {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(pcm).unwrap();
        // Then held open, like a microphone.
        let cmd = format!("cat {}; sleep 30", f.path().display());
        let config = VoiceConfig { record_command: cmd, silence_ms, max_seconds: 10, ..Default::default() };
        (Recorder::detect(&config).unwrap(), f)
    }

    #[test]
    fn stops_on_a_pause_after_speech() {
        let rate = 16_000;
        let audio = pcm(&[(false, 300), (true, 800), (false, 1500)], rate);
        let (rec, _f) = recorder_for(&audio, 600);
        let r = rec.start().unwrap();
        let start = Instant::now();
        while !r.should_stop() {
            assert!(start.elapsed() < Duration::from_secs(8), "never stopped");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(r.heard_speech());
        let wav = r.finish().unwrap();
        assert_eq!(&wav[..4], b"RIFF");
        assert!(wav.len() > 44 + rate as usize, "kept the audio");
    }

    #[test]
    fn silence_is_not_speech() {
        let (rec, _f) = recorder_for(&pcm(&[(false, 1000)], 16_000), 600);
        let r = rec.start().unwrap();
        thread::sleep(Duration::from_millis(400));
        assert!(!r.should_stop(), "no speech yet, so no pause to stop on");
        assert!(r.finish().unwrap_err().to_string().contains("heard nothing"));
    }

    #[test]
    fn a_missing_device_is_explained() {
        let config = VoiceConfig { record_command: "echo 'no such device' >&2; exit 1".into(), ..Default::default() };
        let r = Recorder::detect(&config).unwrap().start().unwrap();
        thread::sleep(Duration::from_millis(200));
        assert!(r.should_stop());
        assert!(r.finish().unwrap_err().to_string().contains("microphone"));
    }
}
