//! Saved conversations: one JSON file per conversation, private to the
//! user, written atomically after every message.
//!
//! A file holds the shared transcript and each model's own history exactly
//! as that model produced it (Claude's thinking blocks and signatures
//! included), so a resumed conversation continues where it left off rather
//! than from a summary.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Mode, Provider};

pub const FORMAT_VERSION: u32 = 1;

/// One turn of the shared transcript.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Turn {
    pub user: String,
    pub provider: Provider,
    /// Never shown to Claude.
    pub private: bool,
    pub reply: String,
    /// One line per tool call: what ran and what came back.
    pub tools: Vec<String>,
}

/// One model's view of the conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedBackend {
    pub model: String,
    /// How many transcript turns it has been told about.
    pub synced: usize,
    /// Its message history in its own wire format.
    pub history: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Conversation {
    pub version: u32,
    pub id: String,
    pub title: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub mode: Mode,
    pub previous: Option<Provider>,
    pub turns: Vec<Turn>,
    /// Keyed by "claude" / "local".
    pub backends: BTreeMap<String, SavedBackend>,
    /// Whose conversation it is ("" in files from before that was kept).
    #[serde(default)]
    pub owner: String,
}

/// What a listing shows.
#[derive(Debug, Clone)]
pub struct Summary {
    pub id: String,
    pub title: String,
    pub updated_ms: u64,
    pub turns: usize,
    /// Whose it is ("" for conversations from before that was kept).
    pub owner: String,
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `$XDG_STATE_HOME/orion/conversations`.
    pub fn default_location() -> Option<Self> {
        crate::Policy::state_dir().map(|d| Self::new(d.join("conversations")))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Write `conv` atomically (temp file, then rename), readable only by
    /// the owner.
    pub fn save(&self, conv: &Conversation) -> Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let tmp = self.dir.join(format!(".{}.json.tmp", conv.id));
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(&serde_json::to_vec(conv)?)?;
        f.sync_all()?;
        fs::rename(&tmp, self.path(&conv.id)).with_context(|| format!("saving conversation {}", conv.id))?;
        Ok(())
    }

    /// Load by id or unique id prefix; an empty `query` means the most
    /// recent conversation.
    pub fn load(&self, query: &str) -> Result<Conversation> {
        let id = self.resolve(query)?;
        let text = fs::read_to_string(self.path(&id)).with_context(|| format!("reading conversation {id}"))?;
        let conv: Conversation = serde_json::from_str(&text).with_context(|| format!("parsing conversation {id}"))?;
        if conv.version > FORMAT_VERSION {
            bail!("conversation {id} was saved by a newer Orion (format {})", conv.version);
        }
        Ok(conv)
    }

    pub fn delete(&self, query: &str) -> Result<String> {
        let id = self.resolve(query)?;
        fs::remove_file(self.path(&id))?;
        Ok(id)
    }

    fn resolve(&self, query: &str) -> Result<String> {
        let all = self.list()?;
        if query.is_empty() {
            return all.into_iter().next().map(|s| s.id).context("there are no saved conversations");
        }
        if all.iter().any(|s| s.id == query) {
            return Ok(query.to_string());
        }
        let matches: Vec<&Summary> = all.iter().filter(|s| s.id.starts_with(query)).collect();
        match matches.as_slice() {
            [one] => Ok(one.id.clone()),
            [] => bail!("no saved conversation matches {query:?}"),
            many => bail!("{query:?} matches {} conversations; give more of the id", many.len()),
        }
    }

    /// Every saved conversation, most recently active first. Unreadable
    /// files are skipped.
    pub fn list(&self) -> Result<Vec<Summary>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.dir.display())),
        };
        let mut out: Vec<Summary> = entries
            .filter_map(|e| {
                let path = e.ok()?.path();
                let name = path.file_name()?.to_str()?;
                if name.starts_with('.') || !name.ends_with(".json") {
                    return None;
                }
                let conv: Conversation = serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()?;
                Some(Summary { id: conv.id, title: conv.title, updated_ms: conv.updated_ms, turns: conv.turns.len(), owner: conv.owner })
            })
            .collect();
        out.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms).then_with(|| b.id.cmp(&a.id)));
        Ok(out)
    }
}

impl Store {
    /// `person`'s most recent conversation. Conversations from before owners
    /// were kept count as `default_owner`'s.
    pub fn latest_for(&self, person: &str, default_owner: &str) -> Result<Option<Conversation>> {
        let owner = |s: &Summary| if s.owner.is_empty() { default_owner.to_string() } else { s.owner.clone() };
        match self.list()?.into_iter().find(|s| owner(s) == person) {
            Some(s) => self.load(&s.id).map(Some),
            None => Ok(None),
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A new id: local date and time, plus a few random-ish hex digits.
pub fn new_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    format!("{}-{:04x}", local_time(now_ms(), "%Y%m%d-%H%M%S"), (nanos ^ std::process::id()) & 0xffff)
}

/// A title from the first message: one line, at most 60 characters.
pub fn title_from(first: &Turn) -> String {
    if first.private {
        return "(private conversation)".into();
    }
    let line = first.user.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= 60 {
        line
    } else {
        format!("{}…", line.chars().take(59).collect::<String>())
    }
}

/// Format a timestamp in the local time zone with strftime `format`.
pub fn local_time(ms: u64, format: &str) -> String {
    let secs = (ms / 1000) as libc::time_t;
    // SAFETY: localtime_r and strftime write only into the buffers given.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return secs.to_string();
        }
        let fmt = std::ffi::CString::new(format).unwrap_or_default();
        let mut buf = [0u8; 64];
        let n = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), fmt.as_ptr(), &tm);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

/// "just now", "5 min ago", "3 h ago", "2 days ago", or a date.
pub fn ago(ms: u64) -> String {
    let secs = now_ms().saturating_sub(ms) / 1000;
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86399 => format!("{} h ago", secs / 3600),
        86400..=604799 => format!("{} days ago", secs / 86400),
        _ => local_time(ms, "%Y-%m-%d"),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use serde_json::json;

    use super::*;

    fn conv(id: &str, updated_ms: u64) -> Conversation {
        Conversation {
            version: FORMAT_VERSION,
            id: id.into(),
            title: format!("about {id}"),
            created_ms: 1,
            updated_ms,
            mode: Mode::Auto,
            previous: Some(Provider::Claude),
            turns: vec![Turn {
                user: "hi".into(),
                provider: Provider::Claude,
                private: false,
                reply: "hello".into(),
                tools: vec![],
            }],
            backends: BTreeMap::from([(
                "claude".to_string(),
                SavedBackend {
                    model: "claude-opus-5-5".into(),
                    synced: 1,
                    history: json!([{"role":"assistant","content":[{"type":"thinking","thinking":"","signature":"sig"}]}]),
                },
            )]),
            owner: String::new(),
        }
    }

    #[test]
    fn saves_lists_loads_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("conversations"));
        assert!(store.list().unwrap().is_empty());
        assert!(store.load("").is_err());

        store.save(&conv("20261020-090000-aaaa", 100)).unwrap();
        store.save(&conv("20261020-100000-bbbb", 300)).unwrap();
        store.save(&conv("20261019-100000-cccc", 200)).unwrap();
        let ids: Vec<String> = store.list().unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["20261020-100000-bbbb", "20261019-100000-cccc", "20261020-090000-aaaa"]);

        assert_eq!(store.load("").unwrap().id, "20261020-100000-bbbb", "empty means latest");
        assert_eq!(store.load("20261019").unwrap(), conv("20261019-100000-cccc", 200), "round trip");
        assert!(store.load("20261020").unwrap_err().to_string().contains("matches 2"));
        assert!(store.load("nope").is_err());

        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(store.dir()), 0o700);
        assert_eq!(mode(&store.path("20261019-100000-cccc")), 0o600);
        assert!(fs::read_dir(store.dir()).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".tmp")));

        assert_eq!(store.delete("20261020-09").unwrap(), "20261020-090000-aaaa");
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn titles_and_times() {
        let turn = |user: &str, private| Turn { user: user.into(), provider: Provider::Local, private, reply: String::new(), tools: vec![] };
        assert_eq!(title_from(&turn("  how full\nis my disk? ", false)), "how full is my disk?");
        assert_eq!(title_from(&turn(&"x".repeat(80), false)).chars().count(), 60);
        assert_eq!(title_from(&turn("my password is hunter2", true)), "(private conversation)");
        assert_eq!(ago(now_ms()), "just now");
        assert_eq!(ago(now_ms() - 7_200_000), "2 h ago");
        assert_eq!(new_id().len(), "20261020-153012-a1b2".len());
    }
}
