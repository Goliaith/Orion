use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};

use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::policy::Policy;
use crate::tool::{parse_args, Risk, Tool};

pub struct FsList;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    path: String,
    #[serde(default = "default_max_entries")]
    max_entries: usize,
}

fn default_max_entries() -> usize {
    500
}

impl Tool for FsList {
    fn name(&self) -> &'static str {
        "fs_list"
    }

    fn description(&self) -> &'static str {
        "List a directory's entries (name, kind, size), sorted by name. Paths must be absolute or start with ~/."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list." },
                "max_entries": { "type": "integer", "minimum": 1, "default": 500 }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: ListArgs = parse_args(args)?;
        let dir = policy.check_read(&a.path)?;
        let mut entries = Vec::new();
        for entry in fs::read_dir(&dir).with_context(|| format!("listing {}", dir.display()))? {
            let entry = entry?;
            let ft = entry.file_type()?;
            let kind = if ft.is_symlink() {
                "symlink"
            } else if ft.is_dir() {
                "dir"
            } else if ft.is_file() {
                "file"
            } else {
                "other"
            };
            let size = if ft.is_file() { entry.metadata().ok().map(|m| m.len()) } else { None };
            entries.push((entry.file_name().to_string_lossy().into_owned(), kind, size));
        }
        entries.sort();
        let total = entries.len();
        entries.truncate(a.max_entries);
        Ok(json!({
            "path": dir,
            "total": total,
            "truncated": total > entries.len(),
            "entries": entries
                .into_iter()
                .map(|(name, kind, size)| json!({ "name": name, "kind": kind, "size": size }))
                .collect::<Vec<_>>(),
        }))
    }
}

pub struct FsRead;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: u64,
    max_bytes: Option<u64>,
}

impl Tool for FsRead {
    fn name(&self) -> &'static str {
        "fs_read"
    }

    fn description(&self) -> &'static str {
        "Read a text file, optionally a byte range of it. Bytes that are not valid UTF-8 are \
         replaced and `utf8_lossy` is set. Use `offset` together with `eof` to page through large files."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File to read." },
                "offset": { "type": "integer", "minimum": 0, "default": 0 },
                "max_bytes": { "type": "integer", "minimum": 1, "description": "Capped by the policy's fs.max_read_bytes." }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: ReadArgs = parse_args(args)?;
        let path = policy.check_read(&a.path)?;
        let meta = fs::metadata(&path)?;
        ensure!(meta.is_file(), "{} is not a regular file", path.display());
        let limit = a.max_bytes.unwrap_or(u64::MAX).min(policy.fs.max_read_bytes);
        let mut f = File::open(&path)?;
        f.seek(SeekFrom::Start(a.offset))?;
        let mut buf = Vec::new();
        f.take(limit).read_to_end(&mut buf)?;
        let bytes_read = buf.len() as u64;
        let (content, lossy) = match String::from_utf8(buf) {
            Ok(s) => (s, false),
            Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
        };
        Ok(json!({
            "path": path,
            "size": meta.len(),
            "offset": a.offset,
            "bytes_read": bytes_read,
            "eof": a.offset + bytes_read >= meta.len(),
            "utf8_lossy": lossy,
            "content": content,
        }))
    }
}

pub struct FsWrite;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
    #[serde(default)]
    append: bool,
    #[serde(default)]
    create_dirs: bool,
}

impl Tool for FsWrite {
    fn name(&self) -> &'static str {
        "fs_write"
    }

    fn description(&self) -> &'static str {
        "Write text to a file inside a writable root, replacing it or (with `append`) adding to it. \
         Set `create_dirs` to create missing parent directories."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" },
                "append": { "type": "boolean", "default": false },
                "create_dirs": { "type": "boolean", "default": false }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: WriteArgs = parse_args(args)?;
        let path = policy.check_write(&a.path)?;
        if a.create_dirs {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
        }
        if a.append {
            use std::io::Write;
            fs::OpenOptions::new().create(true).append(true).open(&path)?.write_all(a.content.as_bytes())?;
        } else {
            fs::write(&path, &a.content)?;
        }
        Ok(json!({ "path": path, "bytes_written": a.content.len(), "appended": a.append }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let policy = Policy::from_toml(&format!("[fs]\nread_roots = []\nwrite_roots = [{root:?}]\n")).unwrap();
        let file = format!("{root}/a/b.txt");

        assert!(FsWrite.call(&policy, json!({ "path": file, "content": "hi" })).is_err());
        FsWrite.call(&policy, json!({ "path": file, "content": "hello ", "create_dirs": true })).unwrap();
        FsWrite.call(&policy, json!({ "path": file, "content": "world", "append": true })).unwrap();

        let r = FsRead.call(&policy, json!({ "path": file, "offset": 6, "max_bytes": 3 })).unwrap();
        assert_eq!(r["content"], "wor");
        assert_eq!(r["eof"], false);

        let l = FsList.call(&policy, json!({ "path": format!("{root}/a") })).unwrap();
        assert_eq!(l["entries"][0]["name"], "b.txt");
        assert_eq!(l["entries"][0]["size"], 11);

        assert!(FsRead.call(&policy, json!({ "path": "/etc/hostname" })).is_err());
        assert!(FsRead.call(&policy, json!({ "path": file, "bogus": 1 })).is_err());
    }
}
