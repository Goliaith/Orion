//! Checkpoints of the dev root, so that what a developer-mode command did
//! to it can be undone (`dev_history`, `dev_rollback`).
//!
//! A checkpoint is a manifest of every file, folder and link in the dev
//! root, with each file's content kept once by its SHA-256 in an object
//! store, in Orion's state folder (which the dev shells never see). Taking
//! one rehashes only the files whose size, time or mode changed since the
//! last, so a checkpoint before each command costs about a walk of the
//! tree. Folders named in `[dev] skip` (build output, downloaded
//! dependencies) and files over `[dev] max_file_mb` are left out, and a
//! rollback leaves them as they are.
//!
//! What a checkpoint can't undo: anything outside the dev root (the
//! toolchain caches, and under `--unsandboxed` everything else), and what
//! a command sent over the network.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::DevConfig;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct FileEntry {
    hash: String,
    size: u64,
    mode: u32,
    mtime_ns: i128,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Manifest {
    id: u64,
    /// Unix seconds.
    ts: u64,
    /// What it was taken before: the command, or a rollback.
    before: String,
    files: BTreeMap<String, FileEntry>,
    dirs: BTreeSet<String>,
    links: BTreeMap<String, String>,
    /// Files left out for their size: a rollback leaves them alone.
    #[serde(default)]
    too_big: BTreeSet<String>,
}

/// One checkpoint, as `dev_history` lists it.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub id: u64,
    pub ts: u64,
    pub before: String,
    pub files: usize,
    pub bytes: u64,
}

/// What a rollback changed.
#[derive(Debug, Default, Serialize)]
pub struct Restored {
    /// The checkpoint taken first, which undoes the rollback.
    pub undo_with: u64,
    pub restored: usize,
    pub removed: usize,
    pub unchanged: usize,
}

/// The checkpoints of one dev root.
pub struct Store {
    dir: PathBuf,
    root: PathBuf,
    keep: usize,
    skip: Vec<String>,
    max_bytes: u64,
}

/// One checkpoint or rollback at a time in this process; the lock file
/// covers other processes.
static BUSY: Mutex<()> = Mutex::new(());

impl Store {
    pub fn new(config: &DevConfig) -> Result<Self> {
        let dir = crate::Policy::state_dir().context("no home folder for checkpoints")?.join("dev-checkpoints");
        Ok(Self::at(dir, config))
    }

    pub(crate) fn at(dir: PathBuf, config: &DevConfig) -> Self {
        Self {
            dir,
            root: config.root(),
            keep: config.checkpoints,
            skip: config.skip.clone(),
            max_bytes: config.max_file_mb.saturating_mul(1 << 20),
        }
    }

    fn lock(&self) -> Result<(std::sync::MutexGuard<'static, ()>, fs::File)> {
        let guard = BUSY.lock().unwrap_or_else(|e| e.into_inner());
        fs::create_dir_all(self.dir.join("objects"))?;
        let file = fs::OpenOptions::new().create(true).truncate(false).write(true).open(self.dir.join("lock"))?;
        // SAFETY: a valid descriptor; flock has no memory preconditions.
        ensure!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0, "locking the checkpoints");
        Ok((guard, file))
    }

    fn manifests(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = fs::read_dir(&self.dir)
            .map(|d| {
                d.flatten()
                    .filter_map(|e| e.file_name().to_str()?.strip_suffix(".json")?.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids
    }

    fn load(&self, id: u64) -> Result<Manifest> {
        let text = fs::read_to_string(self.dir.join(format!("{id}.json"))).with_context(|| format!("there is no checkpoint {id}"))?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Checkpoints, newest first.
    pub fn list(&self) -> Result<Vec<Summary>> {
        let mut out = Vec::new();
        for id in self.manifests().into_iter().rev() {
            let m = self.load(id)?;
            out.push(Summary { id, ts: m.ts, before: m.before.clone(), files: m.files.len(), bytes: m.files.values().map(|f| f.size).sum() });
        }
        Ok(out)
    }

    /// Take a checkpoint of the dev root now. `None` when checkpoints are off.
    pub fn take(&self, before: &str) -> Result<Option<u64>> {
        if self.keep == 0 {
            return Ok(None);
        }
        let _lock = self.lock()?;
        self.take_locked(before).map(Some)
    }

    fn take_locked(&self, before: &str) -> Result<u64> {
        let ids = self.manifests();
        let last = match ids.last() {
            Some(&id) => self.load(id).unwrap_or_default(),
            None => Manifest::default(),
        };
        let mut m = Manifest {
            id: ids.last().map_or(1, |id| id + 1),
            ts: super::now(),
            before: before.chars().take(300).collect(),
            ..Default::default()
        };
        self.walk(&self.root, "", &mut |rel, path, meta| {
            let ft = meta.file_type();
            if ft.is_dir() {
                m.dirs.insert(rel.to_string());
            } else if ft.is_symlink() {
                m.links.insert(rel.to_string(), fs::read_link(path)?.to_string_lossy().into_owned());
            } else if ft.is_file() {
                if meta.len() > self.max_bytes {
                    m.too_big.insert(rel.to_string());
                    return Ok(());
                }
                let entry = FileEntry { hash: String::new(), size: meta.len(), mode: meta.mode() & 0o7777, mtime_ns: mtime_ns(meta) };
                let hash = match last.files.get(rel) {
                    Some(old) if old.size == entry.size && old.mtime_ns == entry.mtime_ns && old.mode == entry.mode => old.hash.clone(),
                    _ => self.store(path)?,
                };
                m.files.insert(rel.to_string(), FileEntry { hash, ..entry });
            }
            Ok(())
        })?;
        let path = self.dir.join(format!("{}.json", m.id));
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec(&m)?)?;
        fs::rename(&tmp, &path)?;
        self.prune()?;
        Ok(m.id)
    }

    /// Walk `dir` (below the dev root, as `rel`), not following links,
    /// leaving out the skipped folders.
    fn walk(&self, dir: &Path, rel: &str, f: &mut dyn FnMut(&str, &Path, &fs::Metadata) -> Result<()>) -> Result<()> {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            let path = e.path();
            let Ok(meta) = fs::symlink_metadata(&path) else { continue };
            if meta.is_dir() && self.skip.contains(&name) {
                continue;
            }
            let child = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            f(&child, &path, &meta)?;
            if meta.is_dir() {
                self.walk(&path, &child, f)?;
            }
        }
        Ok(())
    }

    fn object(&self, hash: &str) -> PathBuf {
        self.dir.join("objects").join(hash)
    }

    /// Keep `path`'s content in the object store; its hash.
    fn store(&self, path: &Path) -> Result<String> {
        let mut file = fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
        let tmp = self.dir.join("objects").join(format!(".tmp-{}", std::process::id()));
        let mut out = fs::File::create(&tmp)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
        }
        let hash: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        let dest = self.object(&hash);
        if dest.exists() {
            fs::remove_file(&tmp)?;
        } else {
            fs::rename(&tmp, &dest)?;
        }
        Ok(hash)
    }

    /// Drop the oldest checkpoints beyond `keep`, and the content only they held.
    fn prune(&self) -> Result<()> {
        let ids = self.manifests();
        if ids.len() <= self.keep {
            return Ok(());
        }
        for id in &ids[..ids.len() - self.keep] {
            fs::remove_file(self.dir.join(format!("{id}.json")))?;
        }
        let mut wanted = BTreeSet::new();
        for id in &ids[ids.len() - self.keep..] {
            wanted.extend(self.load(*id)?.files.into_values().map(|f| f.hash));
        }
        for e in fs::read_dir(self.dir.join("objects"))?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') && !wanted.contains(&name) {
                fs::remove_file(e.path())?;
            }
        }
        Ok(())
    }

    /// Put the dev root back as checkpoint `id` found it. A checkpoint of
    /// it as it is now is taken first, so the rollback can be undone too.
    pub fn rollback(&self, id: u64) -> Result<Restored> {
        ensure!(self.keep > 0, "checkpoints are off ([dev] checkpoints = 0)");
        let _lock = self.lock()?;
        let want = self.load(id)?;
        for e in want.files.values() {
            ensure!(self.object(&e.hash).exists(), "checkpoint {id} is missing content it needs; it can't be put back");
        }
        let undo_with = self.take_locked(&format!("rollback to checkpoint {id}"))?;
        let now = self.load(undo_with)?;
        let mut r = Restored { undo_with, ..Default::default() };

        // What is there now and wasn't then goes, deepest first.
        let mut gone: Vec<&String> = now
            .files
            .keys()
            .chain(now.links.keys())
            .filter(|p| !want.files.contains_key(*p) && !want.links.contains_key(*p) && !want.too_big.contains(*p))
            .collect();
        gone.extend(now.dirs.iter().filter(|d| !want.dirs.contains(*d)));
        gone.sort_by_key(|p| std::cmp::Reverse(p.matches('/').count()));
        for rel in gone {
            let path = self.root.join(rel);
            let meta = fs::symlink_metadata(&path);
            let done = match meta {
                // A folder that still holds skipped content stays.
                Ok(m) if m.is_dir() => fs::remove_dir(&path).or(Ok(())),
                Ok(_) => fs::remove_file(&path),
                Err(_) => Ok(()),
            };
            done.with_context(|| format!("removing {}", path.display()))?;
            r.removed += 1;
        }
        for rel in &want.dirs {
            let path = self.root.join(rel);
            if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
                if fs::symlink_metadata(&path).is_ok() {
                    fs::remove_file(&path)?;
                }
                fs::create_dir_all(&path)?;
            }
        }
        for (rel, target) in &want.links {
            if now.links.get(rel) == Some(target) {
                r.unchanged += 1;
                continue;
            }
            let path = self.root.join(rel);
            remove_any(&path)?;
            std::os::unix::fs::symlink(target, &path)?;
            r.restored += 1;
        }
        for (rel, e) in &want.files {
            if now.files.get(rel).is_some_and(|n| n.hash == e.hash && n.mode == e.mode) {
                r.unchanged += 1;
                continue;
            }
            let path = self.root.join(rel);
            let tmp = path.with_file_name(format!(".orion-restore-{}", std::process::id()));
            fs::copy(self.object(&e.hash), &tmp)?;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(e.mode))?;
            if fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
                fs::remove_dir_all(&path)?;
            }
            fs::rename(&tmp, &path).with_context(|| format!("restoring {}", path.display()))?;
            r.restored += 1;
        }
        Ok(r)
    }
}

fn remove_any(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(_) => {}
    }
    Ok(())
}

fn mtime_ns(meta: &fs::Metadata) -> i128 {
    meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path, keep: usize) -> Store {
        let config = DevConfig { root: dir.join("dev"), checkpoints: keep, ..Default::default() };
        fs::create_dir_all(dir.join("dev")).unwrap();
        Store::at(dir.join("store"), &config)
    }

    #[test]
    fn rolls_back_what_a_command_did() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path(), 10);
        let root = s.root.clone();
        fs::create_dir_all(root.join("app/src")).unwrap();
        fs::write(root.join("app/src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("app/run.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(root.join("app/run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("src/main.rs", root.join("app/link")).unwrap();
        fs::create_dir_all(root.join("app/target/debug")).unwrap();
        fs::write(root.join("app/target/debug/app"), "binary").unwrap();
        let first = s.take("cargo new").unwrap().unwrap();

        // A command that went wrong.
        fs::write(root.join("app/src/main.rs"), "broken").unwrap();
        fs::remove_file(root.join("app/run.sh")).unwrap();
        fs::create_dir_all(root.join("app/junk/deep")).unwrap();
        fs::write(root.join("app/junk/deep/x"), "x").unwrap();
        fs::write(root.join("app/target/debug/app"), "rebuilt").unwrap();

        let r = s.rollback(first).unwrap();
        assert_eq!(fs::read_to_string(root.join("app/src/main.rs")).unwrap(), "fn main() {}\n");
        assert_eq!(fs::metadata(root.join("app/run.sh")).unwrap().mode() & 0o777, 0o755);
        assert!(!root.join("app/junk").exists());
        assert_eq!(fs::read_link(root.join("app/link")).unwrap(), PathBuf::from("src/main.rs"));
        // Skipped folders are left as they are.
        assert_eq!(fs::read_to_string(root.join("app/target/debug/app")).unwrap(), "rebuilt");
        assert!(r.restored >= 2 && r.removed >= 1, "{r:?}");

        // And the rollback itself can be undone.
        s.rollback(r.undo_with).unwrap();
        assert_eq!(fs::read_to_string(root.join("app/src/main.rs")).unwrap(), "broken");
        assert!(root.join("app/junk/deep/x").exists() && !root.join("app/run.sh").exists());
        let list = s.list().unwrap();
        assert_eq!(list.first().unwrap().before, format!("rollback to checkpoint {}", r.undo_with));
    }

    #[test]
    fn keeps_only_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path(), 2);
        for i in 0..4 {
            fs::write(s.root.join("f"), format!("{i}")).unwrap();
            s.take(&format!("step {i}")).unwrap();
        }
        let ids: Vec<u64> = s.list().unwrap().iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![4, 3]);
        assert_eq!(fs::read_dir(dir.path().join("store/objects")).unwrap().count(), 2, "old content is dropped");
        assert!(s.rollback(1).is_err());
        assert!(store(dir.path(), 0).take("x").unwrap().is_none());
    }
}
