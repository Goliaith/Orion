//! The cloud router: when the hosted model behind `[chat.local]` (Gemini's
//! free tier, say) is rate limited, the request goes to the next-best cloud
//! model Orion has a key for instead of waiting, and comes back to the
//! configured model once its limit has passed.
//!
//! Every model here speaks the OpenAI-style API (Claude through Anthropic's
//! compatible endpoint), so the conversation carries over as it is, even in
//! the middle of a turn. Models are ranked by speed and quality. Only a
//! request that is turned away is switched: nothing a model has said is
//! sent twice, and tools still go through the policy, approvals and audit
//! log in the session.

use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use super::http::{secret_env, service, Http};
use super::local::LocalConfig;

/// `[chat.cloud]` in orion.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CloudConfig {
    /// Switch to another cloud model when the configured one is rate
    /// limited, rather than waiting.
    pub enabled: bool,
    /// How the other models are ranked: `balanced` (speed and quality
    /// together), `speed` or `quality` first.
    pub prefer: Prefer,
    /// Which providers may take over: `gemini`, `claude`, `openai`, `groq`,
    /// `openrouter`, `mistral`, `deepseek` or a name of your own. Empty:
    /// every one Orion has a key for.
    pub providers: Vec<String>,
    /// Models to add to the built-in ones, or to rescore them.
    pub models: Vec<CloudModel>,
    /// How long a rate-limited model is left alone when the server doesn't
    /// say.
    pub cooldown_secs: u64,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self { enabled: true, prefer: Prefer::Balanced, providers: Vec::new(), models: Vec::new(), cooldown_secs: 60 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Prefer {
    #[default]
    Balanced,
    Speed,
    Quality,
}

/// One model the router may switch to.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CloudModel {
    pub provider: String,
    pub model: String,
    /// Empty: the provider's.
    pub base_url: String,
    /// Environment variable with the key. Empty: the provider's.
    pub api_key_env: String,
    /// 1 (slow) to 5 (fast).
    pub speed: u8,
    /// 1 (weak) to 5 (strong), at the work Orion does: tools and plain answers.
    pub quality: u8,
    pub reasoning_effort: Option<String>,
}

impl Default for CloudModel {
    fn default() -> Self {
        Self {
            provider: String::new(),
            model: String::new(),
            base_url: String::new(),
            api_key_env: String::new(),
            speed: 3,
            quality: 3,
            reasoning_effort: None,
        }
    }
}

/// Providers Orion knows the address and key variable of.
const PROVIDERS: [(&str, &str, &str); 7] = [
    ("gemini", crate::setup::GEMINI_URL, "GEMINI_API_KEY"),
    ("claude", "https://api.anthropic.com/v1", "ANTHROPIC_API_KEY"),
    ("openai", "https://api.openai.com/v1", "OPENAI_API_KEY"),
    ("groq", "https://api.groq.com/openai/v1", "GROQ_API_KEY"),
    ("openrouter", "https://openrouter.ai/api/v1", "OPENROUTER_API_KEY"),
    ("mistral", "https://api.mistral.ai/v1", "MISTRAL_API_KEY"),
    ("deepseek", "https://api.deepseek.com/v1", "DEEPSEEK_API_KEY"),
];

/// The models tried without any `[chat.cloud]` settings: (provider, model,
/// speed, quality, reasoning effort). Gemini's `-latest` names follow its
/// newest models, and each has a free-tier allowance of its own.
const BUILT_IN: [(&str, &str, u8, u8, Option<&str>); 6] = [
    ("gemini", "gemini-flash-latest", 4, 4, Some("low")),
    ("gemini", "gemini-flash-lite-latest", 5, 3, Some("low")),
    ("gemini", "gemini-pro-latest", 2, 5, Some("low")),
    ("claude", "claude-haiku-5-5", 5, 4, None),
    ("claude", "claude-sonnet-5-5", 3, 5, None),
    ("openai", "gpt-5-mini", 4, 4, Some("low")),
];

/// A model in the lineup, resolved: where it is and how to reach it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key_env: String,
    pub reasoning_effort: Option<String>,
    pub speed: u8,
    pub quality: u8,
}

impl Entry {
    fn same_model(&self, other: &Entry) -> bool {
        let bare = |m: &str| m.strip_prefix("models/").unwrap_or(m).to_string();
        self.base_url.trim_end_matches('/') == other.base_url.trim_end_matches('/') && bare(&self.model) == bare(&other.model)
    }
}

/// The configured model first, then every other model whose key is set
/// (`has_key` says), best first. Only the configured model when there's
/// nothing to switch to, or the router is off.
pub fn lineup(local: &LocalConfig, cloud: &CloudConfig, has_key: &dyn Fn(&str) -> bool) -> Vec<Entry> {
    let home = Entry {
        provider: provider_of(&local.base_url),
        model: local.model.clone(),
        base_url: local.base_url.clone(),
        api_key_env: local.api_key_env.clone(),
        reasoning_effort: local.reasoning_effort.clone(),
        speed: 3,
        quality: 3,
    };
    if !cloud.enabled {
        return vec![home];
    }
    let mut others: Vec<Entry> = BUILT_IN
        .iter()
        .map(|&(provider, model, speed, quality, effort)| CloudModel {
            provider: provider.into(),
            model: model.into(),
            speed,
            quality,
            reasoning_effort: effort.map(str::to_string),
            ..Default::default()
        })
        .chain(cloud.models.iter().cloned())
        .filter_map(resolve)
        .collect();
    // A model of your own replaces a built-in one of the same name.
    let mut i = 0;
    while i < others.len() {
        if others[i + 1..].iter().any(|o| o.same_model(&others[i])) {
            others.remove(i);
        } else {
            i += 1;
        }
    }
    let allowed = |p: &str| cloud.providers.is_empty() || cloud.providers.iter().any(|a| a.eq_ignore_ascii_case(p));
    others.retain(|e| !e.same_model(&home) && allowed(&e.provider) && !e.api_key_env.is_empty() && has_key(&e.api_key_env));
    let score = |e: &Entry| match cloud.prefer {
        Prefer::Balanced => (u16::from(e.speed) + u16::from(e.quality), e.quality),
        Prefer::Speed => (u16::from(e.speed), e.quality),
        Prefer::Quality => (u16::from(e.quality), e.speed),
    };
    others.sort_by_key(|e| std::cmp::Reverse(score(e)));
    let mut all = vec![home];
    all.extend(others);
    all
}

fn resolve(m: CloudModel) -> Option<Entry> {
    let preset = PROVIDERS.iter().find(|(name, _, _)| name.eq_ignore_ascii_case(&m.provider));
    let base_url = match (m.base_url.is_empty(), preset) {
        (false, _) => m.base_url,
        (true, Some((_, url, _))) => url.to_string(),
        (true, None) => {
            tracing::warn!("chat.cloud: {} needs a base_url (provider {:?} isn't one Orion knows)", m.model, m.provider);
            return None;
        }
    };
    let api_key_env = match (m.api_key_env.is_empty(), preset) {
        (false, _) => m.api_key_env,
        (true, Some((_, _, var))) => var.to_string(),
        (true, None) => String::new(),
    };
    if m.model.is_empty() {
        return None;
    }
    let clamp = |n: u8| n.clamp(1, 5);
    Some(Entry {
        provider: m.provider.to_ascii_lowercase(),
        model: m.model,
        base_url,
        api_key_env,
        reasoning_effort: m.reasoning_effort,
        speed: clamp(m.speed),
        quality: clamp(m.quality),
    })
}

/// The provider name for a base URL: a known one, else the service's name.
fn provider_of(base_url: &str) -> String {
    let host = |u: &str| service(u).to_ascii_lowercase();
    PROVIDERS.iter().find(|(_, url, _)| host(url) == host(base_url)).map_or_else(|| host(base_url), |(name, _, _)| name.to_string())
}

/// Whether the key in `var` is set, for [`lineup`].
pub fn key_is_set(var: &str) -> bool {
    secret_env(var).ok().flatten().is_some()
}

/// A model the router can send to.
pub struct Target {
    pub entry: Entry,
    /// Hands rate limits back instead of waiting them out.
    pub http: Http,
    cooling: Option<Instant>,
    /// Why it can't be used at all this session (a key turned down, a
    /// model that doesn't exist).
    pub dead: Option<String>,
}

impl Target {
    pub fn new(entry: Entry, http: Http) -> Self {
        Self { entry, http: http.handing_off(true), cooling: None, dead: None }
    }

    pub fn is_gemini(&self) -> bool {
        service(&self.entry.base_url) == "Gemini"
    }

    /// "gemini-flash-lite-latest (Gemini)"
    pub fn label(&self) -> String {
        format!("{} ({})", self.entry.model, service(&self.entry.base_url))
    }
}

/// Where the next request goes.
#[derive(Debug, PartialEq, Eq)]
pub enum Pick {
    /// To this target, which hands a rate limit back.
    Try(usize),
    /// Every target is limited or already tried: wait on the one that is
    /// free soonest, as before the router.
    Wait(usize),
    /// None can be used.
    None,
}

pub struct CloudRouter {
    /// The configured model first, then the rest best first.
    pub targets: Vec<Target>,
    cooldown: Duration,
}

impl CloudRouter {
    /// A router over `targets` (the configured model first), or `None` when
    /// there is nothing to switch to.
    pub fn new(targets: Vec<Target>, cooldown: Duration) -> Option<Self> {
        (targets.len() > 1).then_some(Self { targets, cooldown: cooldown.max(Duration::from_secs(1)) })
    }

    /// The best target that isn't cooling down or in `tried`.
    pub fn pick(&self, tried: &[usize]) -> Pick {
        let now = Instant::now();
        let usable = |(i, t): &(usize, &Target)| t.dead.is_none() && !tried.contains(i);
        if let Some((i, _)) = self.targets.iter().enumerate().filter(usable).find(|(_, t)| t.cooling.is_none_or(|u| u <= now)) {
            return Pick::Try(i);
        }
        let live = self.targets.iter().enumerate().filter(|(_, t)| t.dead.is_none());
        match live.min_by_key(|(_, t)| t.cooling.unwrap_or(now)) {
            Some((i, _)) => Pick::Wait(i),
            None => Pick::None,
        }
    }

    /// Where a request would go now, for display.
    pub fn next(&self) -> Option<&Target> {
        match self.pick(&[]) {
            Pick::Try(i) | Pick::Wait(i) => self.targets.get(i),
            Pick::None => None,
        }
    }

    /// `i` turned a request away: leave it alone for as long as it asked.
    pub fn limited(&mut self, i: usize, retry_after: Option<Duration>) -> Duration {
        let wait = retry_after.unwrap_or(self.cooldown).max(Duration::from_secs(1));
        self.targets[i].cooling = Some(Instant::now() + wait);
        wait
    }

    /// `i` can't be used this session.
    pub fn failed(&mut self, i: usize, why: String) {
        self.targets[i].dead = Some(why);
    }

    /// How long `i` is still cooling down.
    pub fn cooling_for(&self, i: usize) -> Option<Duration> {
        self.targets[i].cooling.and_then(|u| u.checked_duration_since(Instant::now())).filter(|d| !d.is_zero())
    }
}

/// What Gemini takes in place of a thought signature it didn't write.
const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

/// The history as `model` should get it, after other models took part:
/// a tool call another model made carries no thought signature Gemini would
/// reject, and other providers get none of Gemini's extra fields.
/// `made_by` says which model wrote each assistant message, by index;
/// missing ones are `home`'s.
pub fn messages_for(messages: &[Value], made_by: &[(usize, String)], home: &str, model: &str, gemini: bool) -> Vec<Value> {
    let mut out = messages.to_vec();
    for (i, m) in out.iter_mut().enumerate() {
        let Some(calls) = m.get_mut("tool_calls").and_then(Value::as_array_mut) else { continue };
        let author = made_by.iter().rev().find(|(at, _)| *at == i).map_or(home, |(_, by)| by.as_str());
        if author == model {
            continue;
        }
        for call in calls.iter_mut().filter_map(Value::as_object_mut) {
            if gemini {
                call.insert("extra_content".into(), json!({ "google": { "thought_signature": SKIP_SIGNATURE } }));
            } else {
                call.remove("extra_content");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const GEMINI: &str = crate::setup::GEMINI_URL;

    fn gemini_home(model: &str) -> LocalConfig {
        LocalConfig { base_url: GEMINI.into(), model: model.into(), api_key_env: "GEMINI_API_KEY".into(), ..Default::default() }
    }

    fn models(l: &[Entry]) -> Vec<&str> {
        l.iter().map(|e| e.model.as_str()).collect()
    }

    #[test]
    fn ranks_by_speed_and_quality_after_the_configured_model() {
        let only_gemini = |v: &str| v == "GEMINI_API_KEY";
        let l = lineup(&gemini_home("gemini-2.5-flash"), &CloudConfig::default(), &only_gemini);
        assert_eq!(models(&l), ["gemini-2.5-flash", "gemini-flash-latest", "gemini-flash-lite-latest", "gemini-pro-latest"]);
        assert_eq!(l[0].provider, "gemini");
        assert_eq!(l[1].reasoning_effort.as_deref(), Some("low"));

        let speed = CloudConfig { prefer: Prefer::Speed, ..Default::default() };
        assert_eq!(models(&lineup(&gemini_home("x"), &speed, &only_gemini))[1..], ["gemini-flash-lite-latest", "gemini-flash-latest", "gemini-pro-latest"]);
        let quality = CloudConfig { prefer: Prefer::Quality, ..Default::default() };
        assert_eq!(models(&lineup(&gemini_home("x"), &quality, &only_gemini))[1..], ["gemini-pro-latest", "gemini-flash-latest", "gemini-flash-lite-latest"]);
    }

    #[test]
    fn only_providers_with_a_key() {
        let all = |_: &str| true;
        let l = lineup(&gemini_home("gemini-flash-latest"), &CloudConfig::default(), &all);
        assert_eq!(
            models(&l),
            ["gemini-flash-latest", "claude-haiku-5-5", "claude-sonnet-5-5", "gpt-5-mini", "gemini-flash-lite-latest", "gemini-pro-latest"],
            "the configured model isn't listed twice"
        );
        let claude = lineup(&gemini_home("g"), &CloudConfig::default(), &|v| v == "ANTHROPIC_API_KEY");
        assert_eq!(models(&claude), ["g", "claude-haiku-5-5", "claude-sonnet-5-5"]);
        assert_eq!(claude[1].base_url, "https://api.anthropic.com/v1");
        let none = lineup(&gemini_home("g"), &CloudConfig::default(), &|_| false);
        assert_eq!(models(&none), ["g"]);
        let narrowed = CloudConfig { providers: vec!["Gemini".into()], ..Default::default() };
        assert_eq!(lineup(&gemini_home("g"), &narrowed, &all).len(), 4);
        let off = CloudConfig { enabled: false, ..Default::default() };
        assert_eq!(lineup(&gemini_home("g"), &off, &all).len(), 1);
    }

    #[test]
    fn models_of_your_own() {
        let cloud = CloudConfig {
            models: vec![
                CloudModel { provider: "groq".into(), model: "llama-4".into(), speed: 9, quality: 3, ..Default::default() },
                CloudModel { provider: "gemini".into(), model: "gemini-pro-latest".into(), speed: 1, quality: 1, ..Default::default() },
                CloudModel { provider: "mine".into(), model: "no-url".into(), ..Default::default() },
                CloudModel { provider: "mine".into(), model: "m".into(), base_url: "https://llm.example.net/v1".into(), api_key_env: "MINE".into(), ..Default::default() },
            ],
            ..Default::default()
        };
        let l = lineup(&gemini_home("g"), &cloud, &|_| true);
        let groq = l.iter().find(|e| e.model == "llama-4").unwrap();
        assert_eq!((groq.base_url.as_str(), groq.api_key_env.as_str(), groq.speed), ("https://api.groq.com/openai/v1", "GROQ_API_KEY", 5));
        assert_eq!(l.iter().filter(|e| e.model == "gemini-pro-latest").count(), 1);
        assert_eq!(models(&l).last(), Some(&"gemini-pro-latest"), "rescored to the bottom");
        assert!(!models(&l).contains(&"no-url"));
        assert!(models(&l).contains(&"m"));
    }

    fn router(n: usize) -> CloudRouter {
        let targets = (0..n)
            .map(|i| {
                let e = Entry { provider: "p".into(), model: format!("m{i}"), base_url: format!("http://h{i}"), api_key_env: String::new(), reasoning_effort: None, speed: 3, quality: 3 };
                Target::new(e, Http::new(&format!("http://h{i}"), Vec::new()))
            })
            .collect();
        CloudRouter::new(targets, Duration::from_secs(60)).unwrap()
    }

    #[test]
    fn switches_while_limited_and_comes_back() {
        assert!(CloudRouter::new(Vec::new(), Duration::ZERO).is_none(), "nothing to switch to");
        let mut r = router(3);
        assert_eq!(r.pick(&[]), Pick::Try(0));
        r.limited(0, Some(Duration::from_secs(30)));
        assert_eq!(r.pick(&[]), Pick::Try(1));
        assert!(r.cooling_for(0).is_some_and(|d| d > Duration::from_secs(25)));
        r.limited(1, None);
        r.failed(2, "no such model".into());
        assert_eq!(r.pick(&[]), Pick::Wait(0), "all limited: wait on the one free soonest");
        // A limit that has passed.
        r.targets[0].cooling = Some(Instant::now());
        assert_eq!(r.pick(&[]), Pick::Try(0));
        assert_eq!(r.pick(&[0]), Pick::Wait(0), "each is tried once per request");
        r.failed(0, "x".into());
        r.failed(1, "x".into());
        assert_eq!(r.pick(&[]), Pick::None);
    }

    #[test]
    fn signatures_only_go_back_to_the_model_that_wrote_them() {
        let signed = json!({ "google": { "thought_signature": "abc" } });
        let msgs = vec![
            json!({ "role": "system", "content": "s" }),
            json!({ "role": "assistant", "content": "", "tool_calls": [{ "id": "a", "extra_content": signed }] }),
            json!({ "role": "tool", "tool_call_id": "a", "content": "ok" }),
            json!({ "role": "assistant", "content": "", "tool_calls": [{ "id": "b" }] }),
        ];
        let made_by = vec![(3, "gpt-5-mini".to_string())];
        // Back home: its own signature stays; the other model's call gets the skip value.
        let home = messages_for(&msgs, &made_by, "gemini-3-flash", "gemini-3-flash", true);
        assert_eq!(home[1]["tool_calls"][0]["extra_content"], signed);
        assert_eq!(home[3]["tool_calls"][0]["extra_content"]["google"]["thought_signature"], SKIP_SIGNATURE);
        // Another provider gets no Gemini fields.
        let other = messages_for(&msgs, &made_by, "gemini-3-flash", "claude-haiku-5-5", false);
        assert!(other[1]["tool_calls"][0].get("extra_content").is_none());
        assert_eq!(other[2], msgs[2]);
    }
}
