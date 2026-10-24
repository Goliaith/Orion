//! The AI's journal: things worth finding again later, kept by topic and
//! recalled by what a message is about, instead of being sent with every
//! conversation like the notes in [`super::memory`].
//!
//! The files are HelixCore's (github.com/Goliaith/helixcore, 0.3.0), read
//! and written here directly, so its Python package can search what Orion
//! writes:
//!
//! ```text
//! <home>/.grok/state/semantic/<slug>/memories.jsonl   one entry per line
//! <home>/.grok/state/tasks/<slug>/<category>.json      the latest decision
//! <home>/.grok/state/synaptic/synapses.jsonl           links between slugs
//! ```
//!
//! Each person's topics are the slugs that start with their name (`ana_`),
//! and nobody else's are searched or listed. The home is in Orion's state
//! directory unless `[chat.journal] home` says otherwise; either way the
//! sandbox hides it. Secrets are never kept, and entries the router would
//! call private are only ever given to a model on this machine.

use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::ToolSpec;

/// One entry is a paragraph, not a document.
pub const MAX_ENTRY_CHARS: usize = 1000;
/// HelixCore's slug limit.
const MAX_SLUG_CHARS: usize = 60;
/// Topics named in the prompt, most recently written first.
const PROMPT_TOPICS: usize = 30;
/// Entries per topic that links are worked out from (HelixCore's 25).
const LINK_SAMPLE: usize = 25;

/// `[chat.journal]` in orion.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JournalConfig {
    /// Offer journal_write, journal_decide and journal_recall.
    pub enabled: bool,
    /// Add the entries that best match each message to it, so the model
    /// sees them without spending a request on journal_recall.
    pub auto_recall: bool,
    /// Entries added to a message at most.
    pub recall_limit: usize,
    /// HelixCore's home (`HELIXCORE_HOME`); unset: Orion's state
    /// directory, `~/.local/state/orion/helix`.
    pub home: Option<PathBuf>,
}

impl Default for JournalConfig {
    fn default() -> Self {
        Self { enabled: true, auto_recall: true, recall_limit: 3, home: None }
    }
}

impl JournalConfig {
    /// Where the journal lives, `~` expanded.
    pub fn home(&self) -> Option<PathBuf> {
        match &self.home {
            Some(h) => Some(crate::policy::expand_home(h)),
            None => crate::policy::Policy::state_dir().map(|d| d.join("helix")),
        }
    }
}

/// An entry found by a search.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub topic: String,
    pub text: String,
    pub private: bool,
    pub ts: f64,
    pub score: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Topic {
    pub name: String,
    pub entries: usize,
    /// Of those, the private ones.
    pub private: usize,
    pub updated: f64,
}

/// Two of a person's topics HelixCore found to be about similar things.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub from: String,
    pub to: String,
    pub strength: f64,
}

/// One person's view of a HelixCore journal.
#[derive(Debug, Clone)]
pub struct Journal {
    /// `<home>/.grok/state`.
    root: PathBuf,
    /// `<person>_`, the start of every slug of theirs.
    prefix: String,
}

impl Journal {
    pub fn open(home: &Path, person: &str) -> Self {
        Self { root: home.join(".grok").join("state"), prefix: format!("{}_", slug_name(person)) }
    }

    /// The slug for one of this person's topics. Lenient about how the
    /// model writes it: `Thesis Code` is `thesis-code`.
    fn slug(&self, topic: &str) -> Result<String> {
        let topic = topic.trim().strip_prefix(&self.prefix).unwrap_or(topic.trim());
        let mut name = String::new();
        for c in topic.to_lowercase().chars() {
            match c {
                'a'..='z' | '0'..='9' | '_' => name.push(c),
                '-' | ' ' | '.' | '/' if !name.is_empty() && !name.ends_with('-') => name.push('-'),
                _ => {}
            }
        }
        let name = name.trim_matches(['-', '_']);
        ensure!(!name.is_empty(), "a topic is a short name such as \"thesis-code\" or \"home-network\"");
        let room = MAX_SLUG_CHARS - self.prefix.len();
        ensure!(name.len() <= room, "a topic name is at most {room} characters");
        Ok(format!("{}{name}", self.prefix))
    }

    /// The topic a slug of this person's stands for.
    fn topic_of<'s>(&self, slug: &'s str) -> Option<&'s str> {
        slug.strip_prefix(&self.prefix).filter(|t| !t.is_empty())
    }

    fn memories(&self, slug: &str) -> PathBuf {
        self.root.join("semantic").join(slug).join("memories.jsonl")
    }

    fn synapses(&self) -> PathBuf {
        self.root.join("synaptic").join("synapses.jsonl")
    }

    /// This person's topics, most recently written first.
    pub fn topics(&self) -> Vec<Topic> {
        let mut out: Vec<Topic> = self
            .slugs()
            .into_iter()
            .filter_map(|slug| {
                let entries = read_jsonl(&self.memories(&slug));
                let updated = entries.iter().map(ts).fold(0.0, f64::max);
                let private = entries.iter().filter(|e| is_private(e)).count();
                Some(Topic { name: self.topic_of(&slug)?.to_string(), entries: entries.len(), private, updated })
            })
            .collect();
        out.sort_by(|a, b| b.updated.total_cmp(&a.updated));
        out
    }

    fn slugs(&self) -> Vec<String> {
        let Ok(dir) = std::fs::read_dir(self.root.join("semantic")) else { return Vec::new() };
        let mut out: Vec<String> = dir
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|s| self.topic_of(s).is_some() && self.memories(s).is_file())
            .collect();
        out.sort();
        out
    }

    /// Keep `text` under `topic`. The same text again only freshens the
    /// old entry, as HelixCore does. Returns the topic's slug.
    pub fn write(&self, topic: &str, text: &str, private: bool) -> Result<String> {
        self.write_with(topic, text, private, Map::new())
    }

    fn write_with(&self, topic: &str, text: &str, private: bool, mut metadata: Map<String, Value>) -> Result<String> {
        let slug = self.slug(topic)?;
        let text = text.trim();
        ensure!(!text.is_empty(), "there is nothing to write down");
        ensure!(text.chars().count() <= MAX_ENTRY_CHARS, "an entry is at most {MAX_ENTRY_CHARS} characters; split it up");
        if super::router::contains_secret(text) {
            bail!("that looks like a password, key or token; Orion never keeps secrets in its journal");
        }
        let path = self.memories(&slug);
        let mut entries = read_jsonl(&path);
        if let Some(e) = entries.iter_mut().find(|e| content(e).trim() == text) {
            e["ts"] = json!(now());
            // Once private, always: it may have been said only to the local model.
            if private {
                e["metadata"]["private"] = json!(true);
            }
            rewrite_jsonl(&path, &entries)?;
            return Ok(slug);
        }
        metadata.insert("source".into(), json!("orion"));
        if private {
            metadata.insert("private".into(), json!(true));
        }
        let entry = json!({ "ts": now(), "content": text, "metadata": metadata });
        append_jsonl(&path, &entry)?;
        Ok(slug)
    }

    /// Record a decision and why, as HelixCore's `persist_decision` does:
    /// the latest one per category, an entry in the topic, and a pass over
    /// the links between this person's topics.
    pub fn decide(&self, topic: &str, category: &str, text: &str, private: bool) -> Result<String> {
        let category = category.trim();
        let category = if category.is_empty() { "decisions" } else { category };
        ensure!(
            category.len() <= 40 && category.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "a category is one short word, such as \"decisions\" or \"setup\""
        );
        let mut metadata = Map::new();
        metadata.insert("category".into(), json!(category));
        let slug = self.write_with(topic, text, private, metadata)?;
        let path = self.root.join("tasks").join(&slug).join(format!("{category}.json"));
        let mut record = json!({ "content": text.trim(), "ts": now(), "category": category });
        if private {
            record["private"] = json!(true);
        }
        write_file(&path, serde_json::to_string_pretty(&record)?.as_bytes())?;
        self.link(&slug)?;
        Ok(slug)
    }

    /// The entries best matching `query`, from one topic or all of this
    /// person's, scored as HelixCore's `semantic_search` scores: keyword
    /// overlap for three quarters, recency (gone after 90 days) for one.
    /// Private entries only when `private`.
    pub fn search(&self, topic: Option<&str>, query: &str, limit: usize, private: bool) -> Result<Vec<Hit>> {
        let slugs = match topic {
            Some(t) => vec![self.slug(t)?],
            None => self.slugs(),
        };
        let words: Vec<String> = words(query).filter(|w| w.chars().count() > 2).collect();
        let now = now();
        let mut hits = Vec::new();
        for slug in &slugs {
            let topic = self.topic_of(slug).unwrap_or(slug).to_string();
            for e in read_jsonl(&self.memories(slug)) {
                let hidden = is_private(&e);
                if hidden && !private {
                    continue;
                }
                let score = match words.is_empty() {
                    true => 0.0,
                    false => keyword_score(&searchable(&e), &words) * 0.75,
                } + recency(ts(&e), now) * 0.25;
                hits.push(Hit { topic: topic.clone(), text: content(&e).to_string(), private: hidden, ts: ts(&e), score: round(score, 4) });
            }
        }
        if words.is_empty() {
            hits.sort_by(|a, b| b.ts.total_cmp(&a.ts));
        } else {
            hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(b.ts.total_cmp(&a.ts)));
        }
        hits.truncate(limit);
        Ok(hits)
    }

    /// What to remind the model of for a message: entries sharing at least
    /// two of its words (one, for a one-word message), not counting
    /// little words like "the".
    pub fn recall(&self, message: &str, limit: usize, private: bool) -> Vec<Hit> {
        let mut seen = BTreeSet::new();
        let wanted: Vec<String> =
            words(message).filter(|w| w.chars().count() > 2 && !STOPWORDS.contains(&w.as_str()) && seen.insert(w.clone())).collect();
        if wanted.is_empty() || limit == 0 {
            return Vec::new();
        }
        let needed = wanted.len().min(2);
        let Ok(hits) = self.search(None, &wanted.join(" "), usize::MAX, private) else { return Vec::new() };
        hits.into_iter()
            .filter(|h| {
                let have: BTreeSet<String> = words(&h.text).collect();
                wanted.iter().filter(|w| have.contains(*w)).count() >= needed
            })
            .take(limit)
            .collect()
    }

    /// Links between this person's topics, strongest first.
    pub fn links(&self) -> Vec<Link> {
        let mut latest: HashMap<(String, String), (f64, f64)> = HashMap::new();
        for e in read_jsonl(&self.synapses()) {
            let (Some(from), Some(to)) = (e["from"].as_str(), e["to"].as_str()) else { continue };
            let (Some(from), Some(to)) = (self.topic_of(from), self.topic_of(to)) else { continue };
            let strength = e["strength"].as_f64().unwrap_or(0.0);
            let key = (from.to_string(), to.to_string());
            if latest.get(&key).is_none_or(|(t, _)| ts(&e) >= *t) {
                latest.insert(key, (ts(&e), strength));
            }
        }
        let mut out: Vec<Link> = latest.into_iter().map(|((from, to), (_, strength))| Link { from, to, strength }).collect();
        out.sort_by(|a, b| b.strength.total_cmp(&a.strength).then(a.from.cmp(&b.from)));
        out
    }

    /// HelixCore's synaptogenesis with one new link at most, between this
    /// person's topics only: link `slug` to the topic whose words overlap
    /// its own most (Jaccard 0.12 or more), then strengthen the five
    /// newest of their links by 0.05.
    fn link(&self, slug: &str) -> Result<()> {
        let mut sets: Vec<(String, BTreeSet<String>)> = Vec::new();
        for s in self.slugs() {
            let mut entries = read_jsonl(&self.memories(&s));
            entries.sort_by(|a, b| ts(b).total_cmp(&ts(a)));
            let text = entries.iter().take(LINK_SAMPLE).map(content).collect::<Vec<_>>().join(" ").to_lowercase();
            let set: BTreeSet<String> = link_words(&text).filter(|w| !STOPWORDS.contains(&w.as_str())).collect();
            if set.len() >= 4 {
                sets.push((s, set));
            }
        }
        let path = self.synapses();
        let mut all = read_jsonl(&path);
        let now = now();
        let best = sets.iter().find(|(s, _)| s == slug).and_then(|(_, mine)| {
            sets.iter()
                .filter(|(s, _)| s != slug)
                .map(|(s, theirs)| (s, mine.intersection(theirs).count() as f64 / mine.union(theirs).count().max(1) as f64))
                .filter(|(_, overlap)| *overlap >= 0.12)
                .max_by(|a, b| a.1.total_cmp(&b.1))
        });
        let mut changed = false;
        if let Some((other, overlap)) = best {
            let strength = round((0.5 + overlap).min(0.95), 2);
            let reason = format!("Keyword overlap {overlap:.2} between '{slug}' and '{other}' (local semantic store)");
            match all.iter_mut().rev().find(|e| e["from"] == slug && e["to"] == other.as_str()) {
                Some(e) => {
                    let s = e["strength"].as_f64().unwrap_or(0.0).max(strength);
                    e["strength"] = json!(round(s, 3));
                    e["last_reinforced"] = json!(now);
                }
                None => all.push(json!({
                    "ts": now,
                    "from": slug,
                    "to": other,
                    "strength": round(strength, 3),
                    "type": "semantic-overlap",
                    "reason": reason,
                    "created_by": format!("synaptogenesis_pass@{slug}"),
                    "task_scope": slug,
                    "last_reinforced": now,
                    "usage_count": 0,
                    "metadata": {},
                })),
            }
            changed = true;
        }
        // Strengthen this person's five newest links of 0.4 or more.
        let mut mine: Vec<usize> = (0..all.len())
            .filter(|&i| {
                let e = &all[i];
                let ours = |k: &str| e[k].as_str().is_some_and(|s| self.topic_of(s).is_some());
                ours("from") && ours("to") && e["strength"].as_f64().unwrap_or(0.0) >= 0.4
            })
            .collect();
        mine.sort_by(|&a, &b| ts(&all[b]).total_cmp(&ts(&all[a])));
        for &i in mine.iter().take(5) {
            let e = &mut all[i];
            e["strength"] = json!(round((e["strength"].as_f64().unwrap_or(0.5) + 0.05).min(1.0), 3));
            e["last_reinforced"] = json!(now);
            e["usage_count"] = json!(e["usage_count"].as_u64().unwrap_or(0) + 1);
            changed = true;
        }
        if changed {
            rewrite_jsonl(&path, &all)?;
        }
        Ok(())
    }

    /// The part of a system prompt that names this person's topics. Not
    /// when `private` is false, their private entries: a topic of nothing
    /// else would give away what it is about.
    pub fn prompt(&self, name: &str, private: bool) -> String {
        let topics: Vec<(String, usize)> = self
            .topics()
            .into_iter()
            .map(|t| (t.name, if private { t.entries } else { t.entries - t.private }))
            .filter(|(_, n)| *n > 0)
            .collect();
        if topics.is_empty() {
            return format!(
                "Your journal for {name} is empty. Use journal_write for things worth finding again in later \
                 conversations, and journal_decide for decisions and why they were made."
            );
        }
        let shown: Vec<String> = topics.iter().take(PROMPT_TOPICS).map(|(t, n)| format!("{t} ({n})")).collect();
        let more = topics.len().saturating_sub(PROMPT_TOPICS);
        let mut out = format!("Your journal for {name} has these topics, most recent first: {}", shown.join(", "));
        if more > 0 {
            out.push_str(&format!(", and {more} more"));
        }
        out.push_str(
            ". Entries that match a message are added to it under \"From Orion's journal\"; journal_recall \
             searches it on purpose.",
        );
        out
    }
}

/// A person's part of a slug. Names are letters, digits, `-` and `_`, up
/// to 32 (`voice::speaker::valid_name`); `_` ends the prefix and HelixCore
/// takes ASCII only, so a name with anything else gets a hash of the
/// original too, keeping `ana_b` and `ana-b` apart.
fn slug_name(person: &str) -> String {
    let plain = !person.is_empty()
        && person.len() <= 32
        && person.starts_with(|c: char| c.is_ascii_alphanumeric())
        && person.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if plain {
        return person.to_string();
    }
    let mut name: String = person.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).take(24).collect();
    if !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        name.insert(0, 'p');
    }
    // FNV-1a: stable across builds, unlike std's hasher.
    let hash = person.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193));
    format!("{name}-{:06x}", hash & 0xff_ffff)
}

/// The hits as a note appended to the user's message.
pub fn recall_note(hits: &[Hit]) -> String {
    let now = now();
    let mut out = String::from("[From Orion's journal, matching this message:");
    for h in hits {
        out.push_str(&format!("\n- ({}, {}) {}", h.topic, age(h.ts, now), h.text));
    }
    out.push(']');
    out
}

/// The three journal tools, offered alongside Orion's.
pub fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "journal_write",
            description: "Write something down in your journal for the person you're talking with, under a topic, so \
                you can find it again in a later conversation: what was done and how, what worked, where things are, \
                what is still to do. A paragraph at most. Reuse a topic from the list in your instructions when one \
                fits; otherwise name a new one in a word or two. Not for anything secret (passwords and keys are \
                refused). Short lasting facts about the person themselves belong in memory_save instead.",
            schema: json!({
                "type": "object",
                "properties": {
                    "topic": { "type": "string", "description": "A short name, such as \"thesis-code\" or \"home-network\"." },
                    "text": { "type": "string", "description": "The entry, written so it makes sense on its own later." }
                },
                "required": ["topic", "text"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "journal_decide",
            description: "Record a decision in your journal, with why it was made, under a topic, so a later \
                conversation doesn't undo it or decide it again.",
            schema: json!({
                "type": "object",
                "properties": {
                    "topic": { "type": "string" },
                    "text": { "type": "string", "description": "The decision and the reason for it." },
                    "category": { "type": "string", "description": "One word grouping it, such as \"setup\"; defaults to \"decisions\". A new decision in the same topic and category supersedes the last." }
                },
                "required": ["topic", "text"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "journal_recall",
            description: "Search your journal for the person you're talking with, by keywords, in one topic or all of \
                them. Matches for each message are already added to it, so search when you need something those \
                didn't cover.",
            schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Keywords. Empty lists the newest entries." },
                    "topic": { "type": "string", "description": "Only this topic." }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
    ]
}

pub fn is_journal_tool(name: &str) -> bool {
    matches!(name, "journal_write" | "journal_decide" | "journal_recall")
}

/// Run a journal tool. `private_reason` decides whether a new entry is
/// private; `private` whether private entries may be shown (the model is
/// on this machine).
pub fn call(journal: &Journal, name: &str, input: &Value, private_reason: impl Fn(&str) -> Option<String>, private: bool) -> Result<Value> {
    let text = |key: &str| input[key].as_str().with_context(|| format!("{name} needs {key}"));
    match name {
        "journal_write" => {
            let body = text("text")?;
            let hidden = private_reason(body).is_some();
            let slug = journal.write(text("topic")?, body, hidden)?;
            Ok(json!({ "saved": journal.topic_of(&slug), "private": hidden }))
        }
        "journal_decide" => {
            let body = text("text")?;
            let hidden = private_reason(body).is_some();
            let category = input["category"].as_str().unwrap_or("decisions");
            let slug = journal.decide(text("topic")?, category, body, hidden)?;
            Ok(json!({ "decided": journal.topic_of(&slug), "category": category, "private": hidden }))
        }
        "journal_recall" => {
            let topic = input["topic"].as_str().filter(|t| !t.trim().is_empty());
            let now = now();
            let hits = journal.search(topic, input["query"].as_str().unwrap_or_default(), 5, private)?;
            let hits: Vec<Value> =
                hits.iter().map(|h| json!({ "topic": h.topic, "when": age(h.ts, now), "text": h.text })).collect();
            Ok(json!({ "entries": hits }))
        }
        other => bail!("unknown journal tool {other}"),
    }
}

/// Little words that say nothing about what a message is about.
const STOPWORDS: &[&str] = &[
    "the", "and", "but", "for", "with", "from", "are", "was", "were", "been", "being", "this", "that", "these",
    "those", "its", "you", "they", "them", "our", "your", "their", "have", "has", "had", "does", "did", "not",
    "then", "than", "what", "which", "who", "when", "where", "how", "all", "can", "will", "would", "should",
    "into", "out", "via", "could", "about", "just", "please", "there", "here", "some", "any", "get", "got",
    "want", "need", "like", "know", "make", "let", "now", "today", "also", "too", "very", "really", "much",
    "more", "most", "one", "two", "use", "using", "used", "yes", "okay", "thanks", "thank", "hey", "hello",
];

/// Python's `\w+`, lowercased.
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).map(str::to_lowercase)
}

/// HelixCore's link words, `[a-z][a-z0-9_-]{2,}` over lowercased text.
fn link_words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !matches!(c, 'a'..='z' | '0'..='9' | '_' | '-')).filter_map(|run| {
        let start = run.find(|c: char| c.is_ascii_lowercase())?;
        (run.len() - start >= 3).then(|| run[start..].to_string())
    })
}

/// Occurrences of the query's words in `text`, per query word.
fn keyword_score(text: &str, query: &[String]) -> f64 {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for w in words(text) {
        *counts.entry(w).or_default() += 1;
    }
    query.iter().map(|w| counts.get(w).copied().unwrap_or(0)).sum::<usize>() as f64 / query.len().max(1) as f64
}

fn recency(ts: f64, now: f64) -> f64 {
    (1.0 - (now - ts) / 86400.0 / 90.0).clamp(0.0, 1.0)
}

/// What is searched: the text and its category. HelixCore searches all the
/// metadata, but Orion's own (the source, the private flag) would match
/// every entry.
fn searchable(e: &Value) -> String {
    match e["metadata"]["category"].as_str() {
        Some(c) => format!("{} {c}", content(e)),
        None => content(e).to_string(),
    }
}

fn content(e: &Value) -> &str {
    e["content"].as_str().unwrap_or_default()
}

fn ts(e: &Value) -> f64 {
    e["ts"].as_f64().unwrap_or(0.0)
}

fn is_private(e: &Value) -> bool {
    e["metadata"]["private"].as_bool().unwrap_or(false)
}

fn round(x: f64, places: i32) -> f64 {
    let m = 10f64.powi(places);
    (x * m).round() / m
}

fn now() -> f64 {
    super::store::now_ms() as f64 / 1000.0
}

fn age(ts: f64, now: f64) -> String {
    match ((now - ts) / 86400.0).floor() as i64 {
        ..=0 => "today".into(),
        1 => "yesterday".into(),
        d if d < 60 => format!("{d} days ago"),
        d => format!("{} months ago", d / 30),
    }
}

/// Every entry of a JSONL file; lines that don't parse are skipped, as
/// HelixCore would fail on them, rather than losing the rest.
fn read_jsonl(path: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(Value::is_object)
        .collect()
}

fn line(v: &Value) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string(v)?))
}

fn append_jsonl(path: &Path, entry: &Value) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    make_parent(path)?;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    // A file that was already there (written by HelixCore) may be readable
    // by others.
    f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    f.write_all(line(entry)?.as_bytes())?;
    Ok(())
}

fn rewrite_jsonl(path: &Path, entries: &[Value]) -> Result<()> {
    let mut text = String::new();
    for e in entries {
        text.push_str(&line(e)?);
    }
    write_file(path, text.as_bytes())
}

/// Replace `path` whole: a temporary file, synced, renamed over it.
fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    make_parent(path)?;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn make_parent(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_helixcore_files_and_finds_them_again() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let j = Journal::open(home.path(), "ana");
        assert!(j.topics().is_empty());
        assert_eq!(j.write("Thesis Code", "The thesis code builds with make in ~/code/thesis.", false).unwrap(), "ana_thesis-code");
        j.write("thesis-code", "The plotting scripts need matplotlib 3.9.", false).unwrap();
        j.write("home-network", "The router is at 192.168.1.1 and runs OpenWrt.", false).unwrap();
        // The same text again freshens the entry instead of adding another.
        j.write("thesis-code", "The plotting scripts need matplotlib 3.9.", false).unwrap();

        let path = home.path().join(".grok/state/semantic/ana_thesis-code/memories.jsonl");
        let lines: Vec<Value> = std::fs::read_to_string(&path).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0]["ts"].as_f64().unwrap() > 1.7e9, "seconds, as HelixCore writes them");
        assert_eq!(lines[0]["metadata"]["source"], "orion");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);

        let hits = j.search(None, "matplotlib plotting", 5, false).unwrap();
        assert_eq!(hits[0].text, "The plotting scripts need matplotlib 3.9.");
        assert_eq!(hits[0].topic, "thesis-code");
        assert!(j.search(Some("home-network"), "matplotlib", 5, false).unwrap().iter().all(|h| h.topic == "home-network"));

        let mut topics: Vec<(String, usize)> = j.topics().into_iter().map(|t| (t.name, t.entries)).collect();
        topics.sort();
        assert_eq!(topics, [("home-network".to_string(), 1), ("thesis-code".to_string(), 2)]);
        assert!(j.prompt("Ana", false).contains("thesis-code (2)"), "{}", j.prompt("Ana", false));

        assert!(j.write("x", "the key is sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789", false).is_err());
        assert!(j.write("!!!", "something", false).is_err());
        assert!(j.write("x", &"y".repeat(MAX_ENTRY_CHARS + 1), false).is_err());
    }

    #[test]
    fn reads_what_helixcore_wrote() {
        // As helixcore 0.3.0's write_semantic_memory leaves it.
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".grok/state/semantic/ana_parser-work");
        std::fs::create_dir_all(&dir).unwrap();
        let old = now() - 30.0 * 86400.0;
        std::fs::write(
            dir.join("memories.jsonl"),
            format!(
                "{}\n\n{}\nnot json\n",
                json!({ "ts": old, "content": "Chose recursive descent because the edge cases stayed readable.", "metadata": { "source": "persist_decision", "category": "implementation" } }),
                json!({ "ts": now(), "content": "Tests live in tests/parser.", "metadata": {} }),
            ),
        )
        .unwrap();
        let j = Journal::open(home.path(), "ana");
        let hits = j.search(Some("parser-work"), "recursive descent", 3, false).unwrap();
        // Two of two words once each, 30 days old: 0.75 + 0.25 * (1 - 30/90).
        assert_eq!(hits[0].score, round(0.75 + 0.25 * (2.0 / 3.0), 4));
        assert!(hits[0].text.starts_with("Chose recursive descent"));
        // The category is searched too.
        assert!(j.search(None, "implementation", 1, false).unwrap()[0].text.starts_with("Chose"));
        // No query: newest first.
        assert_eq!(j.search(None, "", 1, false).unwrap()[0].text, "Tests live in tests/parser.");
    }

    #[test]
    fn each_person_and_private_entries_stay_apart() {
        let home = tempfile::tempdir().unwrap();
        let ana = Journal::open(home.path(), "ana");
        let ben = Journal::open(home.path(), "ben");
        ana.write("health", "The physiotherapy appointment moved to Thursdays at the clinic.", true).unwrap();
        ana.write("garden", "The tomato seedlings go out in May after the clinic visit.", false).unwrap();
        ben.write("garden", "Ben waters the tomato beds on Sundays.", false).unwrap();

        let shown = ana.search(None, "clinic thursdays", 5, false).unwrap();
        assert!(shown.iter().all(|h| !h.private && h.topic == "garden"), "{shown:?}");
        assert!(ana.search(None, "clinic thursdays", 5, true).unwrap()[0].private);
        assert!(ben.search(None, "tomato", 5, true).unwrap().iter().all(|h| h.text.starts_with("Ben")));
        assert_eq!(ben.topics().len(), 1);
        // A topic of only private entries isn't named to a model elsewhere.
        assert!(!ana.prompt("Ana", false).contains("health") && ana.prompt("Ana", false).contains("garden (1)"));
        assert!(ana.prompt("Ana", true).contains("health (1)"));
        // People whose names only differ in what a slug can't hold stay apart.
        for (a, b) in [("ana_b", "ana-b"), ("Ana", "ana"), ("józef", "jozef"), ("ana_x", "ana")] {
            let (ja, jb) = (Journal::open(home.path(), a), Journal::open(home.path(), b));
            assert!(!ja.prefix.starts_with(&jb.prefix) && !jb.prefix.starts_with(&ja.prefix), "{a} {b}");
        }
        assert_eq!(slug_name("bea"), "bea");
        assert!(slug_name("ana_b").starts_with("ana-b-"));
        // Writing a private entry again keeps it private, even if it's said
        // in a way that isn't.
        ana.write("health", "The physiotherapy appointment moved to Thursdays at the clinic.", false).unwrap();
        assert!(ana.search(Some("health"), "", 1, true).unwrap()[0].private);
    }

    #[test]
    fn recalls_only_what_a_message_is_about() {
        let home = tempfile::tempdir().unwrap();
        let j = Journal::open(home.path(), "ana");
        j.write("backups", "Backups run nightly with restic to the NAS at /mnt/nas/restic.", false).unwrap();
        j.write("printer", "The printer needs the brlaser driver from the repositories.", false).unwrap();
        assert!(j.recall("hello, how are you today?", 3, false).is_empty(), "little words match nothing");
        assert!(j.recall("is the nas full?", 3, false).is_empty(), "two words, and only one matches");
        assert_eq!(j.recall("nas", 3, false).len(), 1);
        let hits = j.recall("did the restic backups run last night?", 3, false);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].topic, "backups");
        assert!(recall_note(&hits).starts_with("[From Orion's journal, matching this message:\n- (backups, today) Backups run"));
        assert!(j.recall("printer", 3, false)[0].text.contains("brlaser"));
        assert!(j.recall("printer", 0, false).is_empty());
    }

    #[test]
    fn decisions_are_kept_and_link_related_topics() {
        let home = tempfile::tempdir().unwrap();
        let j = Journal::open(home.path(), "ana");
        let other = Journal::open(home.path(), "ben");
        other.write("desktop", "Fedora KDE laptop with Wayland Plasma and the nvidia driver from rpmfusion.", false).unwrap();
        j.write("laptop", "Fedora KDE laptop runs Plasma on Wayland, with the nvidia driver from rpmfusion.", false).unwrap();
        j.decide("display", "setup", "Keep Plasma on Wayland: the nvidia driver from rpmfusion handles the laptop display.", false).unwrap();

        let task: Value =
            serde_json::from_str(&std::fs::read_to_string(home.path().join(".grok/state/tasks/ana_display/setup.json")).unwrap()).unwrap();
        assert_eq!(task["category"], "setup");
        assert!(task["content"].as_str().unwrap().starts_with("Keep Plasma"));
        assert_eq!(j.search(Some("display"), "", 1, false).unwrap()[0].text, task["content"]);

        let links = j.links();
        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!((links[0].from.as_str(), links[0].to.as_str()), ("display", "laptop"));
        assert!(other.links().is_empty(), "never linked to someone else's topic");
        // Another decision strengthens the link rather than adding a row.
        j.decide("display", "setup", "Plasma on Wayland stays; the laptop nvidia driver comes from rpmfusion.", false).unwrap();
        let rows = read_jsonl(&home.path().join(".grok/state/synaptic/synapses.jsonl"));
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0]["usage_count"], 2);
        assert!(rows[0]["strength"].as_f64().unwrap() >= links[0].strength);
        assert!(j.decide("display", "a/b", "x", false).is_err());
    }

    #[test]
    fn tools_respect_privacy() {
        let home = tempfile::tempdir().unwrap();
        let j = Journal::open(home.path(), "ana");
        let private = |t: &str| t.contains("medical").then(|| "mentions \"medical\"".to_string());
        let r = call(&j, "journal_write", &json!({ "topic": "health", "text": "Medical checkup results are in ~/docs/medical." }), private, true).unwrap();
        assert_eq!(r["private"], true);
        call(&j, "journal_decide", &json!({ "topic": "docs", "text": "Scanned documents go in ~/docs, sorted by year." }), private, true).unwrap();
        let remote = call(&j, "journal_recall", &json!({ "query": "docs" }), private, false).unwrap();
        assert_eq!(remote["entries"].as_array().unwrap().len(), 1, "{remote}");
        let local = call(&j, "journal_recall", &json!({ "query": "docs" }), private, true).unwrap();
        assert_eq!(local["entries"].as_array().unwrap().len(), 2, "{local}");
        assert!(call(&j, "journal_write", &json!({ "topic": "x" }), private, true).is_err());
    }
}
