//! Voice tuning: Orion notices when listening goes wrong, says why, and
//! with your approval changes the setting that fixes it.
//!
//! Two things go wrong on real machines: background noise keeps crossing
//! the speech threshold, so a recording runs on to its time limit, and the
//! transcriber writes the wake word as something else ("Oreo"), so Orion
//! never wakes. Orion keeps a little evidence of both in
//! `$XDG_STATE_HOME/orion/voice.json`: for each of the last
//! [`KEPT`] requests, how it ended and its sound levels, and the words heard
//! where the wake word should have been that sound like it. Never audio,
//! and never what was said.
//!
//! [`diagnose`] turns that into suggestions with their reasons. The daemon's
//! watch shows them like its other findings, `voice_check` gives them to the
//! AI, and `voice_tune` applies one. Changes go to
//! `$XDG_CONFIG_HOME/orion/voice-tuning.toml`, which holds only these few
//! settings and is applied over `[voice]`, so the AI can tune listening
//! without being able to touch the rest of the policy.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::record::{rms, Sensitivity, CHUNK_MS};
use super::VoiceConfig;
use crate::policy::Policy;
use crate::tool::{parse_args, Risk, Tool};

/// Requests remembered.
pub const KEPT: usize = 20;
/// The requests a suggestion is based on.
const RECENT: usize = 5;
/// How many of them must have run to the limit.
const LIMITED: usize = 2;
/// How often a near-miss spelling must turn up to be worth an alias.
const MISSES: u32 = 2;

/// The settings `voice_tune` may change, over `[voice]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Tuning {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub noise_margin: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speech_level: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub silence_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_seconds: Option<u64>,
    /// Added to `[voice] wake_aliases`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub wake_aliases: Vec<String>,
}

impl Tuning {
    pub fn path() -> Option<PathBuf> {
        Policy::default_config_path().map(|p| p.with_file_name("voice-tuning.toml"))
    }

    /// The saved tuning; none if there is no file or it doesn't parse.
    pub fn load() -> Self {
        let Some(text) = Self::path().and_then(|p| std::fs::read_to_string(p).ok()) else { return Self::default() };
        toml::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!("ignoring voice-tuning.toml: {e}");
            Self::default()
        })
    }

    fn save(&self) -> Result<PathBuf> {
        let path = Self::path().context("no home directory")?;
        if *self == Self::default() {
            let _ = std::fs::remove_file(&path);
            return Ok(path);
        }
        let text = format!(
            "# Voice settings Orion tuned (voice_tune), applied over [voice] in orion.toml.\n\
             # Delete this file, or ask Orion to reset them, to go back.\n{}",
            toml::to_string(self)?
        );
        write_atomic(&path, &text)?;
        Ok(path)
    }

    pub fn check(&self) -> Result<()> {
        if let Some(m) = self.noise_margin {
            ensure!((1.0..=20.0).contains(&m), "noise_margin goes from 1 to 20");
        }
        if let Some(l) = self.speech_level {
            ensure!((0.0..=10_000.0).contains(&l), "speech_level goes from 0 to 10000");
        }
        if let Some(s) = self.silence_ms {
            ensure!((300..=5000).contains(&s), "silence_ms goes from 300 to 5000");
        }
        if let Some(s) = self.max_seconds {
            ensure!((5..=120).contains(&s), "max_seconds goes from 5 to 120");
        }
        ensure!(self.wake_aliases.len() <= 10, "at most 10 tuned wake aliases");
        for a in &self.wake_aliases {
            ensure!(valid_alias(a), "a wake alias is a word or two of letters, at most 32 characters: {a:?}");
        }
        Ok(())
    }

    pub fn apply(&self, v: &mut VoiceConfig) {
        if let Some(m) = self.noise_margin {
            v.noise_margin = m;
        }
        if let Some(l) = self.speech_level {
            v.speech_level = l;
        }
        if let Some(s) = self.silence_ms {
            v.silence_ms = s;
        }
        if let Some(s) = self.max_seconds {
            v.max_seconds = s;
        }
        for a in &self.wake_aliases {
            if !v.wake_aliases.iter().any(|x| x.eq_ignore_ascii_case(a)) {
                v.wake_aliases.push(a.clone());
            }
        }
    }
}

fn valid_alias(a: &str) -> bool {
    !a.trim().is_empty() && a.len() <= 32 && a.chars().all(|c| c.is_alphabetic() || c == '\'' || c == ' ' || c == '-')
}

pub(crate) fn write_atomic(path: &std::path::Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// How a request's recording ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ended {
    /// A pause after speech, as it should.
    Pause,
    /// The time limit: something kept counting as speech.
    Limit,
    /// Stopped by hand, or the recorder stopped.
    Stopped,
}

/// One request's recording, as levels only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Take {
    pub at: String,
    pub ended: Ended,
    pub secs: f32,
    /// The room's noise floor.
    pub floor: f32,
    /// The speech threshold it was recorded with.
    pub threshold: f32,
    /// The voice: the typical level of what counted as speech.
    pub voice: f32,
    /// The lowest threshold that would have found a pause of `silence_ms`
    /// after speech began: what the noise peaked at in its quietest stretch.
    pub needed: Option<f32>,
}

impl Take {
    pub(crate) fn measure(samples: &[i16], rate: u32, floor: Option<f32>, s: Sensitivity, silence_ms: u64, ended: Ended) -> Option<Self> {
        let chunk = (rate as u64 * CHUNK_MS / 1000) as usize;
        let levels: Vec<f32> = samples.chunks_exact(chunk.max(1)).map(rms).collect();
        let floor = floor.unwrap_or(0.0);
        let threshold = s.threshold(Some(floor));
        let mut loud: Vec<f32> = levels.iter().copied().filter(|&l| l > threshold).collect();
        if loud.is_empty() {
            return None;
        }
        loud.sort_by(f32::total_cmp);
        let voice = loud[loud.len() / 2];
        // From where speech began, the quietest stretch as long as a pause.
        let window = (silence_ms / CHUNK_MS).max(3) as usize;
        let start = levels.iter().position(|&l| l > threshold).unwrap_or(0) + 3;
        let needed = levels
            .get(start..)
            .filter(|rest| rest.len() >= window)
            .and_then(|rest| rest.windows(window).map(|w| w.iter().copied().fold(0.0, f32::max)).min_by(f32::total_cmp));
        Some(Self { at: now(), ended, secs: levels.len() as f32 * CHUNK_MS as f32 / 1000.0, floor, threshold, voice, needed })
    }
}

fn now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    crate::tools::journal_time(secs)
}

/// The evidence kept between runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceLog {
    pub takes: Vec<Take>,
    /// Words heard where the wake word should be that sound like it, and
    /// how often.
    pub near_misses: BTreeMap<String, u32>,
}

impl VoiceLog {
    fn path() -> Option<PathBuf> {
        Policy::state_dir().map(|d| d.join("voice.json"))
    }

    pub fn load() -> Self {
        Self::path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn save(&self) {
        // Tests must not write to the real state directory.
        if cfg!(test) {
            return;
        }
        if let Some(path) = Self::path() {
            if let Err(e) = serde_json::to_string(self).map_err(anyhow::Error::from).and_then(|t| write_atomic(&path, &t)) {
                tracing::debug!("voice log: {e:#}");
            }
        }
    }

    fn update(f: impl FnOnce(&mut Self)) {
        let mut log = Self::load();
        f(&mut log);
        log.save();
    }

    pub fn add_take(take: Take) {
        Self::update(|log| {
            log.takes.push(take);
            let extra = log.takes.len().saturating_sub(KEPT);
            log.takes.drain(..extra);
        });
    }

    pub fn add_near_miss(word: &str) {
        Self::update(|log| {
            *log.near_misses.entry(word.to_string()).or_default() += 1;
            // A handful is plenty; drop the rarest.
            while log.near_misses.len() > 10 {
                let rarest = log.near_misses.iter().min_by_key(|(_, n)| **n).map(|(w, _)| w.clone());
                rarest.map(|w| log.near_misses.remove(&w));
            }
        });
    }
}

/// A change worth making, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Suggestion {
    /// "voice:noise", "voice:wake:oreo": stable while the problem lasts.
    pub key: String,
    pub title: String,
    pub why: String,
    /// The `voice_tune` arguments that fix it; empty when no setting does.
    pub change: Value,
}

fn half_up(x: f32) -> f32 {
    (x * 2.0).ceil() / 2.0
}

/// What the evidence suggests, given the settings in effect.
pub fn diagnose(log: &VoiceLog, config: &VoiceConfig) -> Vec<Suggestion> {
    let mut out = Vec::new();
    let recent = &log.takes[log.takes.len().saturating_sub(RECENT)..];
    let limited: Vec<&Take> = recent.iter().filter(|t| t.ended == Ended::Limit).collect();
    if limited.len() >= LIMITED {
        let n = limited.len();
        let secs = config.max_seconds;
        let floor = median(limited.iter().map(|t| t.floor));
        let voice = median(limited.iter().map(|t| t.voice));
        let threshold = limited.last().map_or(0.0, |t| t.threshold);
        let needed = limited.iter().filter_map(|t| t.needed).fold(0.0, f32::max) * 1.15;
        let mut change = serde_json::Map::new();
        let mut why = format!(
            "{n} of your last {} spoken requests ran on to the {secs}-second limit instead of stopping when you paused. \
             Background noise (up to about {:.0}) kept rising above the speech threshold ({threshold:.0}), so Orion kept listening.",
            recent.len(),
            needed / 1.15
        );
        if needed > 0.0 && needed < voice * 0.6 && floor >= 10.0 {
            let margin = half_up(needed / floor).clamp(1.0, 20.0);
            change.insert("noise_margin".into(), json!(margin));
            why.push_str(&format!(
                " A noise margin of {margin} (now {}) puts the threshold at about {:.0}: above the noise, and still well below your voice (about {voice:.0}).",
                config.noise_margin,
                floor * margin
            ));
        } else if needed > 0.0 && needed < voice * 0.6 {
            let level = ((needed / 10.0).ceil() * 10.0).min(10_000.0);
            change.insert("speech_level".into(), json!(level));
            why.push_str(&format!(" A speech level of {level:.0} (now {:.0}) is above the noise and below your voice (about {voice:.0}).", config.speech_level));
        } else {
            why.push_str(
                " The noise is nearly as loud as your voice, so no threshold tells them apart: move closer to the microphone, \
                 quiet the noise, or use a wake-word detector (voice.wake_command).",
            );
        }
        if secs > 20 {
            change.insert("max_seconds".into(), json!(15));
            why.push_str(" A 15-second limit also stops a runaway recording sooner.");
        }
        out.push(Suggestion { key: "voice:noise".into(), title: "Background noise keeps Orion listening".into(), why, change: Value::Object(change) });
    }
    let phrase = config.wake_phrase.trim();
    for (word, &n) in &log.near_misses {
        if n < MISSES || config.wake_aliases.iter().any(|a| a.eq_ignore_ascii_case(word)) {
            continue;
        }
        out.push(Suggestion {
            key: format!("voice:wake:{word}"),
            title: format!("The wake word is heard as \"{word}\""),
            why: format!(
                "{n} times, the transcriber wrote \"{word}\" at the start of what was said, where \"{phrase}\" would be, \
                 so Orion didn't wake. Adding \"{word}\" as another spelling of the wake word lets it."
            ),
            change: json!({ "add_wake_aliases": [word] }),
        });
    }
    out
}

fn median(values: impl Iterator<Item = f32>) -> f32 {
    let mut v: Vec<f32> = values.collect();
    v.sort_by(f32::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}

pub struct VoiceCheck;

impl Tool for VoiceCheck {
    fn name(&self) -> &'static str {
        "voice_check"
    }

    fn description(&self) -> &'static str {
        "How listening is going: the voice settings in effect, how recent spoken requests ended (a pause, or the time \
         limit because noise kept counting as speech), words the transcriber heard in place of the wake word, and \
         suggested changes with their reasons. Explain a suggestion to the person before applying it with voice_tune."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn call(&self, policy: &Policy, _args: Value) -> Result<Value> {
        let v = &policy.voice;
        let log = VoiceLog::load();
        let recent: Vec<Value> = log
            .takes
            .iter()
            .rev()
            .take(RECENT)
            .map(|t| json!({ "at": t.at, "ended": t.ended, "seconds": t.secs, "noise_floor": t.floor.round(), "threshold": t.threshold.round(), "voice": t.voice.round() }))
            .collect();
        Ok(json!({
            "settings": {
                "noise_margin": v.noise_margin,
                "speech_level": v.speech_level,
                "silence_ms": v.silence_ms,
                "max_seconds": v.max_seconds,
                "wake_phrase": v.wake_phrase,
                "wake_aliases": v.wake_aliases,
                "wake_detector": if v.wake_command.is_empty() { Value::Null } else { json!(v.wake_command) },
            },
            "tuned": Tuning::load(),
            "recent_requests": recent,
            "heard_instead_of_wake_word": log.near_misses,
            "suggestions": diagnose(&log, v),
        }))
    }
}

pub struct VoiceTune;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TuneArgs {
    noise_margin: Option<f32>,
    speech_level: Option<f32>,
    silence_ms: Option<u64>,
    max_seconds: Option<u64>,
    #[serde(default)]
    add_wake_aliases: Vec<String>,
    #[serde(default)]
    remove_wake_aliases: Vec<String>,
    /// Drop every tuned setting: back to orion.toml's.
    #[serde(default)]
    reset: bool,
}

impl TuneArgs {
    fn plan(&self, current: Tuning) -> Result<Tuning> {
        let mut t = if self.reset { Tuning::default() } else { current };
        t.noise_margin = self.noise_margin.or(t.noise_margin);
        t.speech_level = self.speech_level.or(t.speech_level);
        t.silence_ms = self.silence_ms.or(t.silence_ms);
        t.max_seconds = self.max_seconds.or(t.max_seconds);
        for a in &self.add_wake_aliases {
            let a = a.trim().to_lowercase();
            if !t.wake_aliases.contains(&a) {
                t.wake_aliases.push(a);
            }
        }
        t.wake_aliases.retain(|a| !self.remove_wake_aliases.iter().any(|r| r.trim().eq_ignore_ascii_case(a)));
        let changes = self.noise_margin.is_some()
            || self.speech_level.is_some()
            || self.silence_ms.is_some()
            || self.max_seconds.is_some()
            || !self.add_wake_aliases.is_empty()
            || !self.remove_wake_aliases.is_empty();
        ensure!(changes || self.reset, "say what to change: noise_margin, speech_level, silence_ms, max_seconds, add_wake_aliases, remove_wake_aliases or reset");
        t.check()?;
        Ok(t)
    }
}

impl Tool for VoiceTune {
    fn name(&self) -> &'static str {
        "voice_tune"
    }

    fn description(&self) -> &'static str {
        "Change how Orion listens, usually as voice_check suggests: noise_margin (how many times louder than the \
         room's noise speech must be; higher ignores more noise), speech_level (the quietest sound that counts as \
         speech), silence_ms (the pause that ends a request), max_seconds (the longest a request may run), and \
         spellings of the wake word the transcriber writes (add_wake_aliases, remove_wake_aliases). reset goes back \
         to orion.toml's settings. Only these voice settings can change. They apply the next time voice starts."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "noise_margin": { "type": "number", "minimum": 1, "maximum": 20 },
                "speech_level": { "type": "number", "minimum": 0, "maximum": 10000 },
                "silence_ms": { "type": "integer", "minimum": 300, "maximum": 5000 },
                "max_seconds": { "type": "integer", "minimum": 5, "maximum": 120 },
                "add_wake_aliases": { "type": "array", "items": { "type": "string" }, "maxItems": 10 },
                "remove_wake_aliases": { "type": "array", "items": { "type": "string" } },
                "reset": { "type": "boolean", "description": "Drop every tuned setting first." }
            },
            "additionalProperties": false
        })
    }

    fn precheck(&self, _policy: &Policy, args: &Value) -> Result<()> {
        parse_args::<TuneArgs>(args.clone())?.plan(Tuning::load()).map(drop)
    }

    fn call(&self, _policy: &Policy, args: Value) -> Result<Value> {
        let a: TuneArgs = parse_args(args)?;
        let before = Tuning::load();
        let after = a.plan(before.clone())?;
        let path = after.save()?;
        // The evidence for what changed no longer applies.
        VoiceLog::update(|log| {
            if a.reset || a.noise_margin.is_some() || a.speech_level.is_some() || a.silence_ms.is_some() || a.max_seconds.is_some() {
                log.takes.clear();
            }
            for w in &a.add_wake_aliases {
                log.near_misses.remove(&w.trim().to_lowercase());
            }
        });
        Ok(json!({
            "ok": true,
            "before": before,
            "tuned": after,
            "file": path.display().to_string(),
            "applies": "the next time voice starts: restart orion chat or the shell, or `systemctl --user restart orion-voice` for orion listen",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take(ended: Ended, needed: Option<f32>) -> Take {
        Take { at: String::new(), ended, secs: 60.0, floor: 200.0, threshold: 600.0, voice: 5000.0, needed }
    }

    #[test]
    fn noise_that_runs_to_the_limit_raises_the_margin() {
        let log = VoiceLog { takes: vec![take(Ended::Pause, None), take(Ended::Limit, Some(900.0)), take(Ended::Limit, Some(800.0))], ..Default::default() };
        let s = diagnose(&log, &VoiceConfig::default());
        assert_eq!(s.len(), 1, "{s:?}");
        let m = s[0].change["noise_margin"].as_f64().unwrap() as f32;
        assert!(200.0 * m > 900.0 && 200.0 * m < 3000.0, "{m}");
        assert_eq!(s[0].change["max_seconds"], 15);
        assert!(s[0].why.contains("60-second limit"), "{}", s[0].why);
    }

    #[test]
    fn one_long_request_is_not_a_pattern() {
        let log = VoiceLog { takes: vec![take(Ended::Limit, Some(900.0)), take(Ended::Pause, None)], ..Default::default() };
        assert!(diagnose(&log, &VoiceConfig::default()).is_empty());
    }

    #[test]
    fn noise_as_loud_as_the_voice_suggests_no_setting() {
        let log = VoiceLog { takes: vec![take(Ended::Limit, Some(4800.0)), take(Ended::Limit, Some(4900.0))], ..Default::default() };
        let config = VoiceConfig { max_seconds: 15, ..Default::default() };
        let s = diagnose(&log, &config);
        assert_eq!(s[0].change, json!({}));
        assert!(s[0].why.contains("wake-word detector"));
    }

    #[test]
    fn a_repeated_near_miss_becomes_an_alias() {
        let mut log = VoiceLog::default();
        log.near_misses.insert("oreo".into(), 3);
        log.near_misses.insert("onion".into(), 1);
        let s = diagnose(&log, &VoiceConfig::default());
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].change, json!({ "add_wake_aliases": ["oreo"] }));
        let config = VoiceConfig { wake_aliases: vec!["Oreo".into()], ..Default::default() };
        assert!(diagnose(&log, &config).is_empty(), "already an alias");
    }

    #[test]
    fn measures_the_quietest_stretch_after_speech() {
        use super::super::record::tests::pcm;
        // Speech, then noise bursts every 0.5 s: never 1.2 s of quiet.
        let mut parts = vec![(true, 800)];
        for _ in 0..10 {
            parts.extend([(false, 400), (true, 100)]);
        }
        let samples: Vec<i16> = pcm(&parts, 16_000).as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
        let t = Take::measure(&samples, 16_000, Some(3.0), Sensitivity::default(), 1200, Ended::Limit).unwrap();
        assert!(t.needed.unwrap() > t.threshold, "the bursts are what kept it going: {t:?}");
        assert!(t.voice > t.threshold);
    }

    #[test]
    fn tuning_is_bounded_and_merges() {
        let a = TuneArgs { noise_margin: Some(4.5), add_wake_aliases: vec![" Oreo ".into()], ..Default::default() };
        let t = a.plan(Tuning { max_seconds: Some(15), ..Default::default() }).unwrap();
        assert_eq!(t, Tuning { noise_margin: Some(4.5), max_seconds: Some(15), wake_aliases: vec!["oreo".into()], ..Default::default() });
        assert!(TuneArgs { noise_margin: Some(50.0), ..Default::default() }.plan(Tuning::default()).is_err());
        assert!(TuneArgs { add_wake_aliases: vec!["rm -rf /".into()], ..Default::default() }.plan(Tuning::default()).is_err());
        assert!(TuneArgs::default().plan(Tuning::default()).is_err(), "nothing to change");
        let reset = TuneArgs { reset: true, ..Default::default() }.plan(t).unwrap();
        assert_eq!(reset, Tuning::default());

        let mut v = VoiceConfig { wake_aliases: vec!["oryan".into()], ..Default::default() };
        Tuning { noise_margin: Some(4.5), wake_aliases: vec!["oreo".into(), "ORYAN".into()], ..Default::default() }.apply(&mut v);
        assert_eq!(v.noise_margin, 4.5);
        assert_eq!(v.wake_aliases, vec!["oryan".to_string(), "oreo".to_string()]);
    }
}
