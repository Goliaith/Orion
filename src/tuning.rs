//! Self-tuning beyond voice: Orion notices a setting that keeps getting in
//! the way, says why, and with one approval changes it.
//!
//! Three things, from evidence Orion already sees:
//! - the hosted model behind `[chat.local]` keeps rate limiting requests
//!   (Gemini's free tier): a lighter model, or less thinking;
//! - speech takes longer to turn into text than it would to type: a smaller
//!   Whisper model, or the GPU;
//! - a tool is approved every time it asks: pre-approve it.
//!
//! The evidence is in `$XDG_STATE_HOME/orion/tuning.json` (when requests
//! were rate limited, and how long transcriptions took; never what was
//! said) and the audit log (which calls were approved). [`diagnose`] turns
//! it into suggestions. The daemon's watch shows them like its other
//! findings, `settings_check` gives them to the AI, and `settings_tune`
//! applies one by its key, so the values come from the evidence and not from
//! the AI. Changes go to `$XDG_CONFIG_HOME/orion/tuning.toml`, applied over
//! orion.toml, which holds only these few settings. A tool can be
//! pre-approved there only from `ask` to `allow`, only for `write` tools
//! without a pre-approval list of their own, and never over an entry in
//! orion.toml's `[tools]`, so the rest of the policy stays out of the AI's
//! reach. `settings_tune` always asks a person, whatever the policy says.
//! Listening (noise, the wake word) has its own: see `voice::tuning`.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::policy::{Decision, Policy};
use crate::tool::{parse_args, Risk, Tool};
use crate::voice::tuning::{write_atomic, Suggestion};
use crate::voice::VoiceConfig;

/// Rate-limited requests, within [`RATE_WINDOW`], that make a pattern.
const RATE_LIMITED: usize = 3;
const RATE_WINDOW: u64 = 24 * 3600;
/// Transcriptions remembered, and the recent ones a suggestion is based on.
const KEPT: usize = 20;
const RECENT: usize = 5;
const SLOW_TAKES: usize = 3;
/// Slow: transcribing takes at least half as long as the speech, and at
/// least this long.
const SLOW_RATIO: f32 = 0.5;
const SLOW_SECS: f32 = 2.0;
/// Approvals, with no refusals, within [`APPROVAL_WINDOW`], that make a pattern.
const APPROVALS: usize = 5;
const APPROVAL_WINDOW: u64 = 30 * 24 * 3600;
/// The end of the audit log that is read for approvals.
const AUDIT_TAIL: u64 = 2 << 20;

/// Tools with a pre-approval list of their own in orion.toml (commands,
/// units, packages...). Under `allow` most of them run only what is on the
/// list, so `allow` would take away the prompt for the rest: the list is the
/// setting for them.
const SCOPED: &[&str] = &[
    "app_open", "audio_set", "svc_control", "bt_device", "proc_run", "gpu_assign", "power_set", "power_action",
    "clock_set", "flatpak_install", "display_set", "update_apply", "pkg_install",
];
/// Never pre-approved: the tuning tools themselves.
const UNTUNABLE: &[&str] = &["settings_tune", "voice_tune"];
const EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high"];

/// What `settings_tune` may change, over orion.toml.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Overlay {
    #[serde(skip_serializing_if = "ChatTuning::is_empty")]
    pub chat: ChatTuning,
    #[serde(skip_serializing_if = "VoiceTuning::is_empty")]
    pub voice: VoiceTuning,
    /// Pre-approved tools: only `allow`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, Decision>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChatTuning {
    pub local: LocalTuning,
}

impl ChatTuning {
    fn is_empty(&self) -> bool {
        self.local == LocalTuning::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalTuning {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VoiceTuning {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcribe_model: Option<String>,
}

impl VoiceTuning {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

fn valid_model(m: &str) -> bool {
    !m.is_empty() && m.len() <= 100 && m.chars().all(|c| c.is_ascii_alphanumeric() || "._:/-".contains(c))
}

/// A `write` tool without a list of its own, that the policy leaves to `ask`.
pub fn pre_approvable(policy: &Policy, name: &str) -> bool {
    !UNTUNABLE.contains(&name)
        && !SCOPED.contains(&name)
        && !policy.tools.contains_key(name)
        && policy.defaults.write == Decision::Ask
        && crate::tools::builtin().iter().any(|t| t.name() == name && t.risk() == Risk::Write)
}

impl Overlay {
    pub fn path() -> Option<PathBuf> {
        Policy::default_config_path().map(|p| p.with_file_name("tuning.toml"))
    }

    /// The saved tuning; none if there is no file or it doesn't parse.
    pub fn load() -> Self {
        let Some(text) = Self::path().and_then(|p| std::fs::read_to_string(p).ok()) else { return Self::default() };
        toml::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!("ignoring tuning.toml: {e}");
            Self::default()
        })
    }

    fn save(&self) -> Result<PathBuf> {
        let path = Self::path().context("no home directory")?;
        if *self == Self::default() {
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).with_context(|| format!("removing {}", path.display())),
                _ => return Ok(path),
            }
        }
        let text = format!(
            "# Settings Orion tuned with your approval (settings_tune), applied over orion.toml.\n\
             # Delete this file, or ask Orion to undo them, to go back.\n{}",
            toml::to_string(self)?
        );
        write_atomic(&path, &text)?;
        Ok(path)
    }

    pub fn check(&self) -> Result<()> {
        let l = &self.chat.local;
        if let Some(m) = &l.model {
            ensure!(valid_model(m), "a model name is letters, digits and ._:/-, at most 100 characters: {m:?}");
        }
        if let Some(e) = &l.reasoning_effort {
            ensure!(EFFORTS.contains(&e.as_str()), "reasoning_effort is one of {}", EFFORTS.join(", "));
        }
        if let Some(m) = &self.voice.transcribe_model {
            ensure!(valid_model(m), "a model name is letters, digits and ._:/-, at most 100 characters: {m:?}");
        }
        for (name, d) in &self.tools {
            ensure!(*d == Decision::Allow, "tools can only be pre-approved here (\"allow\"), not set to {}", d.as_str());
            ensure!(
                !UNTUNABLE.contains(&name.as_str()) && !SCOPED.contains(&name.as_str()),
                "{name} can't be pre-approved here"
            );
            ensure!(
                crate::tools::builtin().iter().any(|t| t.name() == name && t.risk() == Risk::Write),
                "only write tools can be pre-approved here, and {name} isn't one"
            );
        }
        Ok(())
    }

    /// Apply over orion.toml's settings. A file that fails its checks is
    /// ignored whole; a pre-approval orion.toml decides otherwise is skipped.
    pub fn apply(&self, policy: &mut Policy) {
        if let Err(e) = self.check() {
            tracing::warn!("ignoring tuning.toml: {e:#}");
            return;
        }
        if let Some(m) = &self.chat.local.model {
            policy.chat.local.model = m.clone();
        }
        if let Some(e) = &self.chat.local.reasoning_effort {
            policy.chat.local.reasoning_effort = Some(e.clone());
        }
        if let Some(m) = &self.voice.transcribe_model {
            policy.voice.transcribe_model = m.clone();
        }
        for (name, d) in &self.tools {
            if pre_approvable(policy, name) {
                policy.tools.insert(name.clone(), *d);
            } else {
                tracing::info!("tuning.toml: orion.toml decides {name}, leaving it");
            }
        }
    }

    /// Every tuned setting by its name ("chat.local.model", "tools.fs_write").
    pub fn settings(&self) -> Vec<String> {
        let mut out = Vec::new();
        let l = &self.chat.local;
        l.model.is_some().then(|| out.push("chat.local.model".to_string()));
        l.reasoning_effort.is_some().then(|| out.push("chat.local.reasoning_effort".to_string()));
        self.voice.transcribe_model.is_some().then(|| out.push("voice.transcribe_model".to_string()));
        out.extend(self.tools.keys().map(|t| format!("tools.{t}")));
        out
    }

    fn set(&mut self, name: &str, value: &Value) -> Result<()> {
        let text = || value.as_str().map(str::to_string).with_context(|| format!("{name} takes a string"));
        match name {
            "chat.local.model" => self.chat.local.model = Some(text()?),
            "chat.local.reasoning_effort" => self.chat.local.reasoning_effort = Some(text()?),
            "voice.transcribe_model" => self.voice.transcribe_model = Some(text()?),
            _ if let Some(tool) = name.strip_prefix("tools.") => {
                ensure!(value == "allow", "{name} can only be \"allow\"");
                self.tools.insert(tool.to_string(), Decision::Allow);
            }
            _ => bail!("{name} isn't a setting Orion tunes"),
        }
        Ok(())
    }

    fn unset(&mut self, name: &str) -> bool {
        match name {
            "chat.local.model" => self.chat.local.model.take().is_some(),
            "chat.local.reasoning_effort" => self.chat.local.reasoning_effort.take().is_some(),
            "voice.transcribe_model" => self.voice.transcribe_model.take().is_some(),
            _ => name.strip_prefix("tools.").is_some_and(|t| self.tools.remove(t).is_some()),
        }
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A request the server answered with 429.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimit {
    pub at: u64,
    pub base: String,
    pub model: String,
}

/// How long one recording took to turn into text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transcription {
    pub at: u64,
    /// The transcriber, as [`transcriber_label`] names it.
    pub by: String,
    pub audio_secs: f32,
    pub took_secs: f32,
}

/// The evidence kept between runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Signals {
    pub rate_limits: Vec<RateLimit>,
    pub transcriptions: Vec<Transcription>,
}

impl Signals {
    fn path() -> Option<PathBuf> {
        Policy::state_dir().map(|d| d.join("tuning.json"))
    }

    pub fn load() -> Self {
        Self::path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn update(f: impl FnOnce(&mut Self)) {
        // Tests must not write to the real state directory.
        if cfg!(test) {
            return;
        }
        let mut s = Self::load();
        f(&mut s);
        if let Some(path) = Self::path() {
            if let Err(e) = serde_json::to_string(&s).map_err(anyhow::Error::from).and_then(|t| write_atomic(&path, &t)) {
                tracing::debug!("tuning signals: {e:#}");
            }
        }
    }

    /// A request to `base` for `model` was rate limited.
    pub fn rate_limited(base: &str, model: &str) {
        let at = now();
        Self::update(|s| {
            s.rate_limits.retain(|r| at.saturating_sub(r.at) <= RATE_WINDOW);
            s.rate_limits.push(RateLimit { at, base: base.to_string(), model: model.to_string() });
            let extra = s.rate_limits.len().saturating_sub(50);
            s.rate_limits.drain(..extra);
        });
    }

    pub fn transcribed(by: &str, audio_secs: f32, took: Duration) {
        let t = Transcription { at: now(), by: by.to_string(), audio_secs, took_secs: took.as_secs_f32() };
        Self::update(|s| {
            s.transcriptions.push(t);
            let extra = s.transcriptions.len().saturating_sub(KEPT);
            s.transcriptions.drain(..extra);
        });
    }
}

/// The transcriber `[voice]` sets up, named as `Transcriber::describe` does.
pub fn transcriber_label(v: &VoiceConfig) -> String {
    if v.transcribe_command.is_empty() {
        format!("{} at {}", v.transcribe_model, v.transcribe_url.trim_end_matches('/'))
    } else {
        v.transcribe_command.split_whitespace().next().unwrap_or_default().to_string()
    }
}

/// A call that asked a person, from the audit log.
#[derive(Debug, Clone, PartialEq)]
pub struct Asked {
    pub at: u64,
    pub tool: String,
    pub approved: bool,
}

fn parse_asked(text: &str) -> Vec<Asked> {
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["decision"] == "ask")
        .filter_map(|v| {
            Some(Asked { at: v["ts_ms"].as_u64()? / 1000, tool: v["tool"].as_str()?.to_string(), approved: v["approved"].as_bool()? })
        })
        .collect()
}

/// The recent calls that asked, from the end of the audit log.
pub fn asked(policy: &Policy) -> Vec<Asked> {
    let read = || -> Result<String> {
        let mut f = std::fs::File::open(policy.audit_path().context("no audit log")?)?;
        let len = f.metadata()?.len();
        f.seek(SeekFrom::Start(len.saturating_sub(AUDIT_TAIL)))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        let text = String::from_utf8_lossy(&buf).into_owned();
        // Past the first, partial line.
        Ok(if len > AUDIT_TAIL { text.split_once('\n').map_or(String::new(), |(_, r)| r.to_string()) } else { text })
    };
    read().map(|t| parse_asked(&t)).unwrap_or_default()
}

fn median(values: impl Iterator<Item = f32>) -> f32 {
    let mut v: Vec<f32> = values.collect();
    v.sort_by(f32::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}

/// A Gemini model with a bigger free allowance: Pro to Flash, Flash to Flash-Lite.
fn lighter_gemini(model: &str) -> Option<String> {
    let (prefix, name) = model.strip_prefix("models/").map_or(("", model), |m| ("models/", m));
    if !name.starts_with("gemini-") || name.contains("flash-lite") {
        return None;
    }
    let lighter = match name {
        "gemini-flash-latest" => "gemini-flash-lite-latest".to_string(),
        "gemini-pro-latest" => "gemini-flash-latest".to_string(),
        _ if let Some(v) = name.strip_suffix("-flash") => format!("{v}-flash-lite"),
        _ if let Some(v) = name.strip_suffix("-pro") => format!("{v}-flash"),
        _ => return None,
    };
    Some(format!("{prefix}{lighter}"))
}

/// The next smaller Whisper model, for names that are known to exist:
/// `small`, `whisper-small`, `openai/whisper-small`, `Systran/faster-whisper-small`.
fn smaller_whisper(model: &str) -> Option<String> {
    const SMALLER: &[(&str, &str)] = &[
        ("large-v3-turbo", "small"),
        ("turbo", "small"),
        ("large-v3", "medium"),
        ("large-v2", "medium"),
        ("large-v1", "medium"),
        ("large", "medium"),
        ("medium", "small"),
        ("small", "base"),
        ("base", "tiny"),
    ];
    let (owner, name) = model.rsplit_once('/').map_or((None, model), |(o, n)| (Some(o), n));
    let prefix = match owner {
        None => ["faster-whisper-", "whisper-", ""].into_iter().find(|p| name.starts_with(p))?,
        Some("openai") => "whisper-",
        Some("Systran") => "faster-whisper-",
        Some(_) => return None,
    };
    let size = name.strip_prefix(prefix)?;
    let (size, en) = size.strip_suffix(".en").map_or((size, ""), |s| (s, ".en"));
    let smaller = SMALLER.iter().find(|(from, _)| *from == size)?.1;
    // Systran publishes no turbo.
    if owner == Some("Systran") && size.contains("turbo") {
        return None;
    }
    Some(format!("{}{prefix}{smaller}{en}", owner.map_or(String::new(), |o| format!("{o}/"))))
}

fn rate_limits(policy: &Policy, signals: &Signals, now: u64) -> Option<Suggestion> {
    let local = &policy.chat.local;
    let base = local.base_url.trim_end_matches('/');
    let hits: Vec<&RateLimit> = signals
        .rate_limits
        .iter()
        .filter(|r| r.base == base && now.saturating_sub(r.at) <= RATE_WINDOW && (local.model.is_empty() || r.model == local.model))
        .collect();
    if hits.len() < RATE_LIMITED {
        return None;
    }
    let n = hits.len();
    let model = if local.model.is_empty() { hits[n - 1].model.clone() } else { local.model.clone() };
    let mut change = serde_json::Map::new();
    let why = if base.contains("generativelanguage.googleapis.com") {
        let mut why = format!(
            "{n} requests to {model} in the last day hit Gemini's rate limit, so replies waited or failed. \
             The free tier allows each model only so many requests and tokens a minute, and requests a day."
        );
        if let Some(lighter) = lighter_gemini(&model) {
            why.push_str(&format!(
                " {lighter} has a bigger allowance and answers sooner, though it is less capable on long, tricky jobs."
            ));
            change.insert("chat.local.model".into(), json!(lighter));
        }
        let effort = local.reasoning_effort.as_deref().filter(|e| !e.is_empty());
        if matches!(effort, None | Some("medium" | "high")) {
            why.push_str(&format!(
                " Thinking less (reasoning_effort low, now {}) spends fewer tokens on each request, and they count against the limit too.",
                effort.unwrap_or("the model's default")
            ));
            change.insert("chat.local.reasoning_effort".into(), json!("low"));
        }
        if change.is_empty() {
            why.push_str(" The model and its thinking are already as light as they go: a paid tier, or another provider, has more room.");
        }
        why
    } else {
        format!("{n} requests to {model} at {base} in the last day were rate limited, so replies waited or failed. Its plan or quota is the setting for that.")
    };
    Some(Suggestion { key: "tune:rate-limits".into(), title: format!("{model} keeps hitting its rate limit"), why, change: Value::Object(change) })
}

fn transcription(policy: &Policy, signals: &Signals, gpus: &dyn Fn() -> Vec<String>) -> Option<Suggestion> {
    let v = &policy.voice;
    let by = transcriber_label(v);
    let recent: Vec<&Transcription> =
        signals.transcriptions.iter().rev().filter(|t| t.by == by && t.audio_secs >= 1.0).take(RECENT).collect();
    if recent.len() < SLOW_TAKES {
        return None;
    }
    let ratio = median(recent.iter().map(|t| t.took_secs / t.audio_secs));
    let took = median(recent.iter().map(|t| t.took_secs));
    if ratio < SLOW_RATIO || took < SLOW_SECS {
        return None;
    }
    let pace = if ratio < 1.0 {
        format!("{:.0}% as long as it took to say them", ratio * 100.0)
    } else {
        format!("{ratio:.1} times as long as it took to say them")
    };
    let mut why = format!(
        "Your last {} spoken requests each took about {took:.1}s to turn into text ({pace}), so Orion is slow to answer.",
        recent.len()
    );
    let mut change = serde_json::Map::new();
    if !v.transcribe_command.is_empty() {
        why.push_str(&format!(
            " The transcriber is a command ({by}): a smaller model file in it (ggml-small.bin instead of ggml-medium.bin, say) is faster."
        ));
    } else if let Some(smaller) = smaller_whisper(&v.transcribe_model) {
        why.push_str(&format!(" {smaller} is a smaller Whisper model: much faster, and a little less accurate."));
        change.insert("voice.transcribe_model".into(), json!(smaller));
    }
    if let Some(gpu) = gpus().first() {
        why.push_str(&format!(
            " This machine has a {gpu}: a transcriber built for it (speaches' CUDA image, or whisper.cpp with CUDA or Vulkan) is many times faster than the CPU."
        ));
    }
    Some(Suggestion { key: "tune:transcription".into(), title: "Speech is slow to turn into text".into(), why, change: Value::Object(change) })
}

fn approvals(policy: &Policy, asked: &[Asked], now: u64) -> Vec<Suggestion> {
    let mut by_tool: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for a in asked.iter().filter(|a| now.saturating_sub(a.at) <= APPROVAL_WINDOW) {
        let e = by_tool.entry(&a.tool).or_default();
        if a.approved {
            e.0 += 1;
        } else {
            e.1 += 1;
        }
    }
    by_tool
        .into_iter()
        .filter(|&(tool, (yes, no))| yes >= APPROVALS && no == 0 && pre_approvable(policy, tool))
        .map(|(tool, (yes, _))| Suggestion {
            key: format!("tune:approve:{tool}"),
            title: format!("You approve {tool} every time"),
            why: format!(
                "You approved {tool} all {yes} times it asked in the last 30 days. Pre-approving it lets it run without asking. \
                 Its other limits in orion.toml still apply, every call is still in the audit log, and undoing it brings the question back."
            ),
            change: json!({ format!("tools.{tool}"): "allow" }),
        })
        .collect()
}

/// What the evidence suggests, given the settings in effect. `gpus` names
/// the machine's discrete GPUs; it is only asked when transcription is slow.
pub fn diagnose(policy: &Policy, signals: &Signals, asked: &[Asked], gpus: &dyn Fn() -> Vec<String>, now: u64) -> Vec<Suggestion> {
    let mut out: Vec<Suggestion> = rate_limits(policy, signals, now).into_iter().collect();
    out.extend(transcription(policy, signals, gpus));
    out.extend(approvals(policy, asked, now));
    out
}

fn discrete_gpus() -> Vec<String> {
    use crate::tools::gpu::{inventory, Kind};
    inventory().into_iter().filter(|g| g.kind == Kind::Discrete).map(|g| g.name).collect()
}

/// [`diagnose`] on this machine's evidence.
pub fn current(policy: &Policy) -> Vec<Suggestion> {
    diagnose(policy, &Signals::load(), &asked(policy), &discrete_gpus, now())
}

pub struct SettingsCheck;

impl Tool for SettingsCheck {
    fn name(&self) -> &'static str {
        "settings_check"
    }

    fn description(&self) -> &'static str {
        "Settings that keep getting in the way, with the change that fixes each and why: the chat model hitting \
         its rate limit (a lighter model, less thinking), slow speech to text (a smaller Whisper model, the GPU), \
         and tools the person approves every time (pre-approve them). Also what Orion has tuned so far. Explain a \
         suggestion to the person before applying it with settings_tune. Listening (noise, the wake word) is voice_check's."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn call(&self, policy: &Policy, _args: Value) -> Result<Value> {
        let signals = Signals::load();
        let asked = asked(policy);
        let t = now();
        let base = policy.chat.local.base_url.trim_end_matches('/');
        let mut approvals: BTreeMap<&str, Value> = BTreeMap::new();
        for a in asked.iter().filter(|a| t.saturating_sub(a.at) <= APPROVAL_WINDOW) {
            let e = approvals.entry(&a.tool).or_insert_with(|| json!({ "approved": 0, "declined": 0 }));
            let k = if a.approved { "approved" } else { "declined" };
            e[k] = json!(e[k].as_u64().unwrap_or(0) + 1);
        }
        let by = transcriber_label(&policy.voice);
        let transcriptions: Vec<Value> = signals
            .transcriptions
            .iter()
            .rev()
            .filter(|x| x.by == by)
            .take(RECENT)
            .map(|x| json!({ "audio_seconds": x.audio_secs, "took_seconds": x.took_secs }))
            .collect();
        Ok(json!({
            "settings": {
                "chat.local.base_url": policy.chat.local.base_url,
                "chat.local.model": policy.chat.local.model,
                "chat.local.reasoning_effort": policy.chat.local.reasoning_effort,
                "voice.transcriber": by,
            },
            "tuned": Overlay::load().settings(),
            "rate_limited_last_day": signals.rate_limits.iter().filter(|r| r.base == base && t.saturating_sub(r.at) <= RATE_WINDOW).count(),
            "recent_transcriptions": transcriptions,
            "approvals_last_30_days": approvals,
            "suggestions": diagnose(policy, &signals, &asked, &discrete_gpus, t),
        }))
    }
}

pub struct SettingsTune;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TuneArgs {
    /// A suggestion's key, from settings_check.
    apply: Option<String>,
    /// Tuned settings to put back to orion.toml's.
    #[serde(default)]
    undo: Vec<String>,
    #[serde(default)]
    reset: bool,
}

impl TuneArgs {
    fn plan(&self, current: Overlay, suggestions: impl FnOnce() -> Vec<Suggestion>) -> Result<Overlay> {
        let given = usize::from(self.apply.is_some()) + usize::from(!self.undo.is_empty()) + usize::from(self.reset);
        ensure!(given == 1, "give one of apply (a suggestion's key from settings_check), undo (tuned settings) or reset");
        let mut t = if self.reset { Overlay::default() } else { current };
        if let Some(key) = &self.apply {
            let all = suggestions();
            let Some(s) = all.iter().find(|s| &s.key == key) else {
                let keys: Vec<&str> = all.iter().map(|s| s.key.as_str()).collect();
                bail!("{key} isn't a current suggestion; settings_check lists them (now: {})", if keys.is_empty() { "none".into() } else { keys.join(", ") });
            };
            let change = s.change.as_object().filter(|c| !c.is_empty());
            let change = change.with_context(|| format!("no setting fixes {key}: {}", s.why))?;
            for (name, value) in change {
                t.set(name, value)?;
            }
        }
        for name in &self.undo {
            ensure!(t.unset(name), "{name} isn't tuned (tuned: {})", t.settings().join(", "));
        }
        t.check()?;
        Ok(t)
    }
}

impl Tool for SettingsTune {
    fn name(&self) -> &'static str {
        "settings_tune"
    }

    fn description(&self) -> &'static str {
        "Make a change settings_check suggests, by its key (apply), put tuned settings back to orion.toml's \
         (undo, by name such as \"chat.local.model\" or \"tools.fs_write\"), or drop them all (reset). The values \
         come from the suggestion, not from you. It can change only the chat model and its reasoning effort, the \
         transcription model, and pre-approve write tools the person always approves; never anything else in the \
         policy. It always asks the person first."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "apply": { "type": "string", "description": "A suggestion's key, such as tune:rate-limits." },
                "undo": { "type": "array", "items": { "type": "string" }, "description": "Tuned settings to undo." },
                "reset": { "type": "boolean", "description": "Undo every tuned setting." }
            },
            "additionalProperties": false
        })
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        parse_args::<TuneArgs>(args.clone())?.plan(Overlay::load(), || current(policy)).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: TuneArgs = parse_args(args)?;
        let before = Overlay::load();
        let after = a.plan(before.clone(), || current(policy))?;
        let path = after.save()?;
        // The evidence for what changed no longer applies.
        let (chat, voice) = (before.chat != after.chat, before.voice != after.voice);
        Signals::update(|s| {
            if chat {
                s.rate_limits.clear();
            }
            if voice {
                s.transcriptions.clear();
            }
        });
        Ok(json!({
            "ok": true,
            "before": before.settings(),
            "tuned": after,
            "file": path.display().to_string(),
            "applies": "the chat model and speech to text the next time orion chat, the shell or orion listen starts; \
                        a pre-approval as soon as the daemon reloads (a few seconds), or a chat's next start without the daemon",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GEMINI: &str = "https://generativelanguage.googleapis.com/v1beta/openai";

    fn gemini(model: &str, effort: Option<&str>) -> Policy {
        let effort = effort.map_or(String::new(), |e| format!("reasoning_effort = \"{e}\"\n"));
        Policy::from_toml(&format!("[chat.local]\nbase_url = \"{GEMINI}/\"\nmodel = \"{model}\"\n{effort}")).unwrap()
    }

    fn limited(n: usize, model: &str, at: u64) -> Signals {
        Signals { rate_limits: (0..n).map(|_| RateLimit { at, base: GEMINI.into(), model: model.into() }).collect(), ..Default::default() }
    }

    fn none() -> Vec<String> {
        Vec::new()
    }

    #[test]
    fn repeated_rate_limits_suggest_a_lighter_gemini() {
        let p = gemini("gemini-2.5-flash", None);
        let s = diagnose(&p, &limited(3, "gemini-2.5-flash", 1000), &[], &none, 2000);
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!(s[0].change, json!({ "chat.local.model": "gemini-2.5-flash-lite", "chat.local.reasoning_effort": "low" }));
        assert!(s[0].why.contains("3 requests"), "{}", s[0].why);

        assert!(diagnose(&p, &limited(2, "gemini-2.5-flash", 1000), &[], &none, 2000).is_empty(), "two is not a pattern");
        assert!(diagnose(&p, &limited(5, "gemini-2.5-flash", 1000), &[], &none, 1000 + RATE_WINDOW + 1).is_empty(), "old ones");
        assert!(diagnose(&p, &limited(5, "gemini-2.5-pro", 1000), &[], &none, 2000).is_empty(), "another model's");

        let lite = gemini("gemini-2.5-flash-lite", Some("none"));
        let s = diagnose(&lite, &limited(4, "gemini-2.5-flash-lite", 1000), &[], &none, 2000);
        assert_eq!(s[0].change, json!({}), "nothing lighter");
        assert!(s[0].why.contains("paid tier"));
    }

    #[test]
    fn lighter_gemini_models() {
        assert_eq!(lighter_gemini("gemini-2.5-pro").as_deref(), Some("gemini-2.5-flash"));
        assert_eq!(lighter_gemini("models/gemini-2.5-flash").as_deref(), Some("models/gemini-2.5-flash-lite"));
        assert_eq!(lighter_gemini("gemini-flash-latest").as_deref(), Some("gemini-flash-lite-latest"));
        assert_eq!(lighter_gemini("gemini-2.5-flash-preview-05-20"), None, "previews have no lite twin");
        assert_eq!(lighter_gemini("qwen3:8b"), None);
    }

    #[test]
    fn smaller_whisper_models() {
        assert_eq!(smaller_whisper("Systran/faster-whisper-large-v3").as_deref(), Some("Systran/faster-whisper-medium"));
        assert_eq!(smaller_whisper("Systran/faster-whisper-medium.en").as_deref(), Some("Systran/faster-whisper-small.en"));
        assert_eq!(smaller_whisper("openai/whisper-large-v3-turbo").as_deref(), Some("openai/whisper-small"));
        assert_eq!(smaller_whisper("base").as_deref(), Some("tiny"));
        assert_eq!(smaller_whisper("tiny"), None);
        assert_eq!(smaller_whisper("Systran/faster-distil-whisper-large-v3"), None);
        assert_eq!(smaller_whisper("deepdml/faster-whisper-large-v3-turbo-ct2"), None);
        assert_eq!(smaller_whisper("whisper-1"), None, "OpenAI's hosted one");
    }

    fn voice(model: &str) -> Policy {
        Policy::from_toml(&format!("[voice]\ntranscribe_url = \"http://127.0.0.1:8000/v1\"\ntranscribe_model = \"{model}\"\n")).unwrap()
    }

    fn takes(by: &str, audio: f32, took: f32, n: usize) -> Signals {
        let t = Transcription { at: 0, by: by.into(), audio_secs: audio, took_secs: took };
        Signals { transcriptions: vec![t; n], ..Default::default() }
    }

    #[test]
    fn slow_transcription_suggests_a_smaller_model() {
        let p = voice("Systran/faster-whisper-medium");
        let by = transcriber_label(&p.voice);
        let gpu = || vec!["NVIDIA GeForce RTX 3060".to_string()];
        let s = diagnose(&p, &takes(&by, 4.0, 6.0, 3), &[], &gpu, 0);
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!(s[0].change, json!({ "voice.transcribe_model": "Systran/faster-whisper-small" }));
        assert!(s[0].why.contains("1.5 times") && s[0].why.contains("RTX 3060"), "{}", s[0].why);

        assert!(diagnose(&p, &takes(&by, 4.0, 1.0, 5), &[], &gpu, 0).is_empty(), "fast enough");
        assert!(diagnose(&p, &takes(&by, 4.0, 6.0, 2), &[], &gpu, 0).is_empty(), "too few");
        assert!(diagnose(&p, &takes("another", 4.0, 6.0, 5), &[], &gpu, 0).is_empty(), "another transcriber's");
    }

    #[test]
    fn the_label_matches_the_transcriber() {
        let p = voice("small");
        let t = crate::voice::Transcriber::from_config(&p.voice).unwrap();
        assert_eq!(t.describe(), transcriber_label(&p.voice));
    }

    fn asks(tool: &str, yes: usize, no: usize) -> Vec<Asked> {
        let a = |approved| Asked { at: 100, tool: tool.into(), approved };
        (0..yes).map(|_| a(true)).chain((0..no).map(|_| a(false))).collect()
    }

    #[test]
    fn a_tool_approved_every_time_can_be_pre_approved() {
        let p = Policy::from_toml("").unwrap();
        let s = diagnose(&p, &Signals::default(), &asks("fs_write", 5, 0), &none, 200);
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!(s[0].key, "tune:approve:fs_write");
        assert_eq!(s[0].change, json!({ "tools.fs_write": "allow" }));

        assert!(diagnose(&p, &Signals::default(), &asks("fs_write", 9, 1), &none, 200).is_empty(), "declined once");
        assert!(diagnose(&p, &Signals::default(), &asks("fs_write", 4, 0), &none, 200).is_empty(), "too few");
        assert!(diagnose(&p, &Signals::default(), &asks("proc_run", 9, 0), &none, 200).is_empty(), "runs anything");
        assert!(diagnose(&p, &Signals::default(), &asks("pkg_install", 9, 0), &none, 200).is_empty(), "has its own list");
        assert!(diagnose(&p, &Signals::default(), &asks("settings_tune", 9, 0), &none, 200).is_empty(), "itself");
        let asked = Policy::from_toml("[tools]\nfs_write = \"ask\"\n").unwrap();
        assert!(diagnose(&asked, &Signals::default(), &asks("fs_write", 9, 0), &none, 200).is_empty(), "orion.toml says ask");
    }

    #[test]
    fn reads_approvals_from_the_audit_log() {
        let log = [
            json!({ "ts_ms": 5000, "tool": "fs_write", "decision": "ask", "approved": true }),
            json!({ "ts_ms": 6000, "tool": "fs_write", "decision": "ask", "approved": null }),
            json!({ "ts_ms": 7000, "tool": "fs_read", "decision": "allow", "approved": null }),
            json!({ "ts_ms": 8000, "tool": "fs_mkdir", "decision": "ask", "approved": false }),
        ]
        .map(|v| v.to_string())
        .join("\n");
        assert_eq!(
            parse_asked(&format!("{log}\nnot json")),
            vec![Asked { at: 5, tool: "fs_write".into(), approved: true }, Asked { at: 8, tool: "fs_mkdir".into(), approved: false }]
        );
    }

    #[test]
    fn applying_takes_the_suggestions_values_and_undoes() {
        let rate = Suggestion {
            key: "tune:rate-limits".into(),
            title: String::new(),
            why: String::new(),
            change: json!({ "chat.local.model": "gemini-2.5-flash-lite", "chat.local.reasoning_effort": "low" }),
        };
        let approve = Suggestion { key: "tune:approve:fs_write".into(), title: String::new(), why: String::new(), change: json!({ "tools.fs_write": "allow" }) };
        let all = || vec![rate.clone(), approve.clone()];
        let apply = |k: &str| TuneArgs { apply: Some(k.into()), ..Default::default() };

        let t = apply("tune:rate-limits").plan(Overlay::default(), all).unwrap();
        let t = apply("tune:approve:fs_write").plan(t, all).unwrap();
        assert_eq!(t.settings(), ["chat.local.model", "chat.local.reasoning_effort", "tools.fs_write"]);
        assert!(apply("tune:approve:proc_run").plan(t.clone(), all).is_err(), "not a suggestion");
        let none = Suggestion { change: json!({}), ..rate.clone() };
        assert!(apply("tune:rate-limits").plan(Overlay::default(), || vec![none.clone()]).is_err(), "nothing to apply");
        assert!(TuneArgs::default().plan(Overlay::default(), all).is_err(), "nothing asked");

        let undo = TuneArgs { undo: vec!["tools.fs_write".into()], ..Default::default() }.plan(t.clone(), all).unwrap();
        assert_eq!(undo.settings(), ["chat.local.model", "chat.local.reasoning_effort"]);
        assert!(TuneArgs { undo: vec!["voice.transcribe_model".into()], ..Default::default() }.plan(t.clone(), all).is_err());
        assert_eq!(TuneArgs { reset: true, ..Default::default() }.plan(t, all).unwrap(), Overlay::default());
    }

    #[test]
    fn the_overlay_cannot_reach_the_rest_of_the_policy() {
        let parse = |s: &str| toml::from_str::<Overlay>(s);
        assert!(parse("[fs]\nwrite_roots = [\"/\"]\n").is_err(), "only its own settings");
        assert!(parse("[chat.local]\nbase_url = \"http://evil\"\n").is_err());
        for bad in ["[tools]\nproc_run = \"allow\"\n", "[tools]\nfs_write = \"deny\"\n", "[tools]\nsettings_tune = \"allow\"\n", "[tools]\nfs_read = \"allow\"\n", "[tools]\nsvc_control = \"allow\"\n"] {
            assert!(parse(bad).unwrap().check().is_err(), "{bad}");
        }
        assert!(parse("[chat.local]\nreasoning_effort = \"max; rm\"\n").unwrap().check().is_err());

        let o = parse("[chat.local]\nmodel = \"gemini-2.5-flash-lite\"\n[voice]\ntranscribe_model = \"small\"\n[tools]\nfs_write = \"allow\"\nfs_mkdir = \"allow\"\n").unwrap();
        let mut p = Policy::from_toml("[tools]\nfs_mkdir = \"deny\"\n").unwrap();
        o.apply(&mut p);
        assert_eq!(p.chat.local.model, "gemini-2.5-flash-lite");
        assert_eq!(p.voice.transcribe_model, "small");
        assert_eq!(p.decide("fs_write", Risk::Write), Decision::Allow);
        assert_eq!(p.decide("fs_mkdir", Risk::Write), Decision::Deny, "orion.toml's say wins");

        let mut strict = Policy::from_toml("[defaults]\nwrite = \"deny\"\n").unwrap();
        o.apply(&mut strict);
        assert_eq!(strict.decide("fs_write", Risk::Write), Decision::Deny, "not over a stricter default");
        assert_eq!(toml::from_str::<Overlay>(&toml::to_string(&o).unwrap()).unwrap(), o);
    }

    #[test]
    fn tuning_always_asks() {
        let p = Policy::from_toml("[defaults]\nwrite = \"allow\"\n[tools]\nsettings_tune = \"allow\"\n").unwrap();
        assert_eq!(p.decide("settings_tune", Risk::Write), Decision::Ask);
        let p = Policy::from_toml("[tools]\nsettings_tune = \"deny\"\n").unwrap();
        assert_eq!(p.decide("settings_tune", Risk::Write), Decision::Deny);
    }
}
