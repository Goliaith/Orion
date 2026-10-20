//! What the AI remembers about each person, across conversations: their
//! preferences, their projects, how they like things done. The model saves
//! and forgets notes with two tools, and is given the notes of whoever it
//! is talking to at the start of their conversations.
//!
//! One file per person, readable only by its owner. Secrets are never
//! kept, and notes that the router would call private are only ever given
//! to the local model.

use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::ToolSpec;

/// Enough for what matters, small enough to send with every conversation.
pub const MAX_NOTES: usize = 60;
pub const MAX_NOTE_CHARS: usize = 300;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Note {
    pub id: u32,
    pub text: String,
    /// Only the local model sees it.
    pub private: bool,
    pub created_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Memory {
    pub person: String,
    pub notes: Vec<Note>,
    next_id: u32,
    #[serde(skip)]
    path: Option<PathBuf>,
}

/// Where memories live: `$XDG_STATE_HOME/orion/memory`.
pub fn default_dir() -> Option<PathBuf> {
    crate::policy::Policy::state_dir().map(|d| d.join("memory"))
}

impl Memory {
    /// `person`'s memory in `dir` (empty if there is none yet).
    pub fn load(dir: &Path, person: &str) -> Result<Self> {
        let path = dir.join(format!("{person}.json"));
        let mut m = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Memory>(&text).with_context(|| format!("{} is not a memory file", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Memory { person: person.into(), ..Default::default() },
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        m.person = person.into();
        m.path = Some(path);
        Ok(m)
    }

    fn save(&self) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let path = self.path.as_ref().context("this memory has no file")?;
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        let tmp = path.with_extension("tmp");
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Remember something. `private` keeps it from Claude.
    pub fn add(&mut self, text: &str, private: bool) -> Result<Note> {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        ensure!(!text.is_empty(), "there is nothing to remember");
        ensure!(text.chars().count() <= MAX_NOTE_CHARS, "a note is at most {MAX_NOTE_CHARS} characters; keep it to one fact");
        if super::router::contains_secret(&text) {
            bail!("that looks like a password, key or token; Orion never keeps secrets in memory");
        }
        if let Some(n) = self.notes.iter().find(|n| n.text.eq_ignore_ascii_case(&text)) {
            return Ok(n.clone());
        }
        ensure!(
            self.notes.len() < MAX_NOTES,
            "memory is full ({MAX_NOTES} notes); forget something that no longer matters first"
        );
        self.next_id = self.next_id.max(self.notes.iter().map(|n| n.id).max().unwrap_or(0)) + 1;
        let note = Note { id: self.next_id, text, private, created_ms: super::store::now_ms() };
        self.notes.push(note.clone());
        self.save()?;
        Ok(note)
    }

    pub fn forget(&mut self, id: u32) -> Result<Note> {
        let at = self.notes.iter().position(|n| n.id == id).with_context(|| format!("there is no note {id}"))?;
        let note = self.notes.remove(at);
        self.save()?;
        Ok(note)
    }

    pub fn clear(&mut self) -> Result<usize> {
        let n = self.notes.len();
        self.notes.clear();
        self.save()?;
        Ok(n)
    }

    /// The part of a system prompt that tells the model what it remembers
    /// about this person. Private notes only when `private` (the local model).
    pub fn prompt(&self, name: &str, private: bool) -> String {
        let shown: Vec<&Note> = self.notes.iter().filter(|n| private || !n.private).collect();
        let withheld = self.notes.len() - shown.len();
        let mut out = format!("You are talking with {name}.");
        if shown.is_empty() {
            out.push_str(" You don't remember anything about them yet.");
        } else {
            out.push_str(&format!(" What you remember about {name} from earlier conversations (note ids are for memory_forget):"));
            for n in shown {
                out.push_str(&format!("\n- [{}] {}", n.id, n.text));
            }
        }
        if withheld > 0 {
            out.push_str(&format!("\n({withheld} private notes are kept for the local model only.)"));
        }
        out
    }
}

/// The two memory tools, offered alongside Orion's.
pub fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "memory_save",
            description: "Remember one lasting fact about the person you're talking with, for future conversations: a \
                preference, their name for something, a project and where it lives, how they like things done. One \
                short fact per note. Not for passing details or anything secret (passwords and keys are refused). \
                Tell them when you save something.",
            schema: json!({
                "type": "object",
                "properties": { "text": { "type": "string", "description": "The fact, written so it makes sense on its own later." } },
                "required": ["text"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "memory_forget",
            description: "Forget a note about the person you're talking with, by its id: when they ask you to, or when it \
                is no longer true (then save the new fact).",
            schema: json!({
                "type": "object",
                "properties": { "id": { "type": "integer" } },
                "required": ["id"],
                "additionalProperties": false
            }),
        },
    ]
}

pub fn is_memory_tool(name: &str) -> bool {
    matches!(name, "memory_save" | "memory_forget")
}

/// Run a memory tool for `memory`. `private_reason` decides whether a new
/// note is private.
pub fn call(memory: &mut Memory, name: &str, input: &Value, private_reason: impl Fn(&str) -> Option<String>) -> Result<Value> {
    match name {
        "memory_save" => {
            let text = input["text"].as_str().context("memory_save needs text")?;
            let private = private_reason(text).is_some();
            let note = memory.add(text, private)?;
            Ok(json!({ "saved": note.id, "text": note.text, "private": note.private, "notes": memory.notes.len() }))
        }
        "memory_forget" => {
            let id = input["id"].as_u64().context("memory_forget needs an id")? as u32;
            let note = memory.forget(id)?;
            Ok(json!({ "forgot": note.id, "text": note.text }))
        }
        other => bail!("unknown memory tool {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_forgets_and_keeps_secrets_out() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut m = Memory::load(dir.path(), "ana").unwrap();
        assert!(m.notes.is_empty());
        let a = m.add("Prefers  metric units", false).unwrap();
        let b = m.add("Her thesis code is in ~/code/thesis", false).unwrap();
        assert_eq!((a.id, b.id), (1, 2));
        assert_eq!(a.text, "Prefers metric units");
        // The same fact twice is one note.
        assert_eq!(m.add("prefers metric units", false).unwrap().id, 1);
        assert!(m.add("Her API key is sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789", false).is_err());
        assert!(m.add(&"x".repeat(MAX_NOTE_CHARS + 1), false).is_err());

        let path = dir.path().join("ana.json");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let mut back = Memory::load(dir.path(), "ana").unwrap();
        assert_eq!(back.notes.len(), 2);
        assert_eq!(back.forget(1).unwrap().text, "Prefers metric units");
        assert!(back.forget(1).is_err());
        // Ids are never reused.
        assert_eq!(back.add("Likes short answers", false).unwrap().id, 3);
        // Someone else's memory is separate.
        assert!(Memory::load(dir.path(), "ben").unwrap().notes.is_empty());
    }

    #[test]
    fn private_notes_stay_with_the_local_model() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = Memory::load(dir.path(), "ana").unwrap();
        let private = |t: &str| t.contains("medical").then(|| "mentions \"medical\"".to_string());
        call(&mut m, "memory_save", &json!({ "text": "Prefers metric units" }), private).unwrap();
        let r = call(&mut m, "memory_save", &json!({ "text": "Has a medical appointment every Tuesday" }), private).unwrap();
        assert_eq!(r["private"], true);
        let for_claude = m.prompt("Ana", false);
        assert!(for_claude.contains("[1] Prefers metric units") && !for_claude.contains("medical"), "{for_claude}");
        assert!(for_claude.contains("1 private notes are kept for the local model only"));
        assert!(m.prompt("Ana", true).contains("[2] Has a medical appointment"));
        assert!(Memory::load(dir.path(), "ben").unwrap().prompt("Ben", true).contains("don't remember anything"));
        assert!(call(&mut m, "memory_forget", &json!({ "id": 9 }), private).is_err());
    }
}
