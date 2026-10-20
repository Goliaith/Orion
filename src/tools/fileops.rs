//! Copying, moving, deleting and finding files, inside the policy's roots.
//!
//! Changes stay inside the writable roots. A symlink is always handled as
//! the link itself, never followed to what it points to, so a link inside
//! a root can't be used to move, delete or overwrite something outside it.
//! Deleting moves to the Trash (the freedesktop one your file manager
//! shows) unless asked to delete for good.

use std::collections::VecDeque;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::journal::local_time;
use crate::policy::{expand_home, Policy};
use crate::tool::{parse_args, Risk, Tool};

/// Most entries one copy, move or search goes through.
const MAX_ENTRIES: usize = 100_000;

/// `raw` as an entry inside a writable root, without following it if it is
/// a symlink: its folder resolved and checked, its own name kept. Never a
/// writable root itself.
fn entry_in_write_roots(policy: &Policy, raw: &str) -> Result<PathBuf> {
    let path = expand_home(Path::new(raw));
    ensure!(path.is_absolute(), "path must be absolute or start with ~/: {raw}");
    let name = path.file_name().with_context(|| format!("{raw} doesn't name a file or folder"))?;
    let parent = path.parent().context("no folder")?;
    let parent = policy.check_write(parent.to_str().context("the path isn't text")?)?;
    let entry = parent.join(name);
    ensure!(!policy.fs.write_roots.contains(&entry), "{} is a writable root itself", entry.display());
    Ok(entry)
}

fn kind(meta: &fs::Metadata) -> &'static str {
    let t = meta.file_type();
    if t.is_symlink() {
        "symlink"
    } else if t.is_dir() {
        "dir"
    } else if t.is_file() {
        "file"
    } else {
        "other"
    }
}

/// Where `from` lands for `to`: inside `to` when it is a real folder (not a
/// link to one), as `cp` and `mv` do; else `to` itself.
fn destination(policy: &Policy, from: &Path, to: &str) -> Result<PathBuf> {
    let to = entry_in_write_roots(policy, to)?;
    match fs::symlink_metadata(&to) {
        Ok(m) if m.is_dir() => {
            let name = from.file_name().context("no name to copy")?;
            Ok(to.join(name))
        }
        _ => Ok(to),
    }
}

/// Copy `from` to `to` (which doesn't exist), folders recursively, keeping
/// symlinks as links. (entries, bytes) copied.
fn copy_tree(from: &Path, to: &Path) -> Result<(usize, u64)> {
    let (mut entries, mut bytes) = (0usize, 0u64);
    let mut queue = VecDeque::from([(from.to_path_buf(), to.to_path_buf())]);
    while let Some((src, dst)) = queue.pop_front() {
        entries += 1;
        ensure!(entries <= MAX_ENTRIES, "more than {MAX_ENTRIES} entries; that's more than Orion copies at once");
        let meta = fs::symlink_metadata(&src)?;
        let t = meta.file_type();
        if t.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&src)?, &dst).with_context(|| format!("linking {}", dst.display()))?;
        } else if t.is_dir() {
            fs::create_dir(&dst).with_context(|| format!("creating {}", dst.display()))?;
            fs::set_permissions(&dst, meta.permissions())?;
            for e in fs::read_dir(&src)? {
                let e = e?;
                queue.push_back((e.path(), dst.join(e.file_name())));
            }
        } else if t.is_file() {
            bytes += fs::copy(&src, &dst).with_context(|| format!("copying {}", src.display()))?;
        } else {
            bail!("{} is a device, socket or pipe; Orion copies files, folders and links", src.display());
        }
    }
    Ok((entries, bytes))
}

fn remove_tree(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    // remove_dir_all doesn't follow symlinks inside the folder.
    if meta.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) }
        .with_context(|| format!("removing {}", path.display()))
}

/// Refuse to put a folder inside itself.
fn not_into_itself(from: &Path, to: &Path) -> Result<()> {
    ensure!(!to.starts_with(from), "{} is inside {}", to.display(), from.display());
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CopyArgs {
    from: String,
    to: String,
    #[serde(default)]
    overwrite: bool,
}

fn check_copy(policy: &Policy, a: &CopyArgs, moving: bool) -> Result<(PathBuf, PathBuf)> {
    let from = if moving {
        let f = entry_in_write_roots(policy, &a.from)?;
        fs::symlink_metadata(&f).with_context(|| format!("there is no {}", f.display()))?;
        f
    } else {
        policy.check_read(&a.from)?
    };
    let to = destination(policy, &from, &a.to)?;
    not_into_itself(&from, &to)?;
    ensure!(from != to, "{} is already there", from.display());
    if let Ok(m) = fs::symlink_metadata(&to) {
        ensure!(a.overwrite, "{} exists; set overwrite to replace it", to.display());
        ensure!(!m.is_dir(), "{} is a folder; Orion doesn't replace folders, delete it first", to.display());
    }
    Ok((from, to))
}

fn copy_schema(verb: &str) -> Value {
    json!({
        "type": "object",
        "properties": {
            "from": { "type": "string", "description": format!("The file or folder to {verb}.") },
            "to": { "type": "string", "description": "The new path, or a folder to put it in." },
            "overwrite": { "type": "boolean", "default": false, "description": "Replace a file already there (never a folder)." }
        },
        "required": ["from", "to"],
        "additionalProperties": false
    })
}

pub struct FsCopy;

impl Tool for FsCopy {
    fn name(&self) -> &'static str {
        "fs_copy"
    }

    fn description(&self) -> &'static str {
        "Copy a file or folder (with everything in it) to a new path, or into a folder, inside a writable root. \
         Symlinks are copied as links. Nothing already there is replaced unless `overwrite` is set."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        copy_schema("copy")
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        check_copy(policy, &parse_args(args.clone())?, false).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: CopyArgs = parse_args(args)?;
        let (from, to) = check_copy(policy, &a, false)?;
        if fs::symlink_metadata(&to).is_ok() {
            fs::remove_file(&to)?;
        }
        let (entries, bytes) = copy_tree(&from, &to)?;
        Ok(json!({ "from": from, "to": to, "entries": entries, "bytes": bytes }))
    }
}

pub struct FsMove;

impl Tool for FsMove {
    fn name(&self) -> &'static str {
        "fs_move"
    }

    fn description(&self) -> &'static str {
        "Move or rename a file or folder, within the writable roots: to a new path, or into a folder. Works across \
         filesystems (by copying, then removing the original). Nothing already there is replaced unless `overwrite` is set."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        copy_schema("move")
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        check_copy(policy, &parse_args(args.clone())?, true).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: CopyArgs = parse_args(args)?;
        let (from, to) = check_copy(policy, &a, true)?;
        let how = match fs::rename(&from, &to) {
            Ok(()) => "renamed",
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                if fs::symlink_metadata(&to).is_ok() {
                    fs::remove_file(&to)?;
                }
                copy_tree(&from, &to)?;
                remove_tree(&from)?;
                "copied to the other filesystem, then removed"
            }
            Err(e) => return Err(e).with_context(|| format!("moving {} to {}", from.display(), to.display())),
        };
        Ok(json!({ "from": from, "to": to, "how": how }))
    }
}

pub struct FsDelete;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteArgs {
    path: String,
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    permanent: bool,
}

fn check_delete(policy: &Policy, a: &DeleteArgs) -> Result<(PathBuf, fs::Metadata)> {
    let path = entry_in_write_roots(policy, &a.path)?;
    let meta = fs::symlink_metadata(&path).with_context(|| format!("there is no {}", path.display()))?;
    if meta.is_dir() && !a.recursive {
        let empty = fs::read_dir(&path).map(|mut d| d.next().is_none()).unwrap_or(false);
        ensure!(empty, "{} is a folder with things in it; set recursive to delete it all", path.display());
    }
    Ok((path, meta))
}

/// The user's Trash, as the freedesktop spec has it.
fn trash_dir() -> Result<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("no home folder for the Trash")?;
    Ok(data.join("Trash"))
}

/// `/a b/ç` → `/a%20b/%C3%A7`, as .trashinfo files hold paths.
fn percent_encode(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str()
        .as_bytes()
        .iter()
        .map(|&b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Move `path` to the Trash. Where it went.
fn trash(path: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let trash = trash_dir()?;
    for sub in ["files", "info"] {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(trash.join(sub))?;
    }
    let name = path.file_name().context("no name")?.to_string_lossy().into_owned();
    let mut n = 1;
    let (dest, info) = loop {
        let candidate = if n == 1 { name.clone() } else { format!("{name}.{n}") };
        let info = trash.join("info").join(format!("{candidate}.trashinfo"));
        // create_new: two deletes at once can't take the same name.
        match fs::OpenOptions::new().write(true).create_new(true).open(&info) {
            Ok(mut f) => {
                use std::io::Write;
                let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
                let date = local_time(now).replace(' ', "T");
                write!(f, "[Trash Info]\nPath={}\nDeletionDate={date}\n", percent_encode(path))?;
                break (trash.join("files").join(&candidate), info);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e).context("writing to the Trash"),
        }
    };
    if let Err(e) = fs::rename(path, &dest) {
        let _ = fs::remove_file(&info);
        if e.raw_os_error() == Some(libc::EXDEV) {
            bail!("{} is on another filesystem than your Trash; set permanent to delete it for good", path.display());
        }
        return Err(e).with_context(|| format!("moving {} to the Trash", path.display()));
    }
    Ok(dest)
}

impl Tool for FsDelete {
    fn name(&self) -> &'static str {
        "fs_delete"
    }

    fn description(&self) -> &'static str {
        "Delete a file or folder inside a writable root, by moving it to the Trash (where the file manager can \
         restore it). A folder with things in it needs `recursive`; `permanent` deletes for good instead."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "recursive": { "type": "boolean", "default": false, "description": "Also a folder with things in it." },
                "permanent": { "type": "boolean", "default": false, "description": "Delete for good, not to the Trash." }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        check_delete(policy, &parse_args(args.clone())?).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: DeleteArgs = parse_args(args)?;
        let (path, meta) = check_delete(policy, &a)?;
        let mut out = json!({ "path": path, "kind": kind(&meta) });
        if a.permanent {
            remove_tree(&path)?;
            out["deleted"] = json!("for good");
        } else {
            out["trashed_to"] = json!(trash(&path)?);
        }
        Ok(out)
    }
}

pub struct FsMkdir;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MkdirArgs {
    path: String,
}

impl Tool for FsMkdir {
    fn name(&self) -> &'static str {
        "fs_mkdir"
    }

    fn description(&self) -> &'static str {
        "Create a folder inside a writable root, with any missing folders above it."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        policy.check_write(&parse_args::<MkdirArgs>(args.clone())?.path).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: MkdirArgs = parse_args(args)?;
        let path = policy.check_write(&a.path)?;
        let existed = path.is_dir();
        fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok(json!({ "path": path, "created": !existed }))
    }
}

pub struct FsFind;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindArgs {
    path: String,
    name: Option<String>,
    contains: Option<String>,
    kind: Option<String>,
    #[serde(default)]
    hidden: bool,
    #[serde(default = "default_depth")]
    max_depth: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_depth() -> usize {
    10
}

fn default_limit() -> usize {
    100
}

/// A name pattern, ignoring case: `*` and `?` wildcards, or else a part of the name.
fn name_matches(pattern: &str, name: &str) -> bool {
    let (p, n) = (pattern.to_lowercase(), name.to_lowercase());
    if !p.contains(['*', '?']) {
        return n.contains(&p);
    }
    let (p, t): (Vec<char>, Vec<char>) = (p.chars().collect(), n.chars().collect());
    // The usual backtracking: on a mismatch, let the last `*` take one more.
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || (p[pi] != '*' && p[pi] == t[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// The first line of a small text file holding `needle`: (line number, line).
fn find_in(path: &Path, needle: &str) -> Option<(usize, String)> {
    let mut f = fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.by_ref().take(1 << 20).read_to_end(&mut buf).ok()?;
    if buf.contains(&0) {
        return None; // binary
    }
    let text = String::from_utf8_lossy(&buf);
    text.lines().enumerate().find(|(_, l)| l.to_lowercase().contains(needle)).map(|(i, l)| {
        let l = l.trim();
        (i + 1, if l.chars().count() > 200 { l.chars().take(200).chain("…".chars()).collect() } else { l.to_string() })
    })
}

impl Tool for FsFind {
    fn name(&self) -> &'static str {
        "fs_find"
    }

    fn description(&self) -> &'static str {
        "Find files and folders under a folder: by name (ignoring case; `*` and `?` wildcards, or else any part of \
         the name), by kind, and by text they contain (small text files). Skips hidden files unless `hidden` is \
         set, and doesn't follow symlinks."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The folder to search under." },
                "name": { "type": "string", "description": "\"*.pdf\", \"report\", \"IMG_20??*\"." },
                "contains": { "type": "string", "description": "Text the file holds, ignoring case (files up to 1 MiB)." },
                "kind": { "type": "string", "enum": ["file", "dir"] },
                "hidden": { "type": "boolean", "default": false },
                "max_depth": { "type": "integer", "minimum": 1, "maximum": 50, "default": 10 },
                "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 100 }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: FindArgs = parse_args(args)?;
        ensure!((1..=1000).contains(&a.limit) && (1..=50).contains(&a.max_depth), "limit is 1 to 1000, max_depth 1 to 50");
        ensure!(a.kind.as_deref().is_none_or(|k| k == "file" || k == "dir"), "kind is file or dir");
        let needle = a.contains.as_ref().map(|c| c.to_lowercase());
        ensure!(needle.as_ref().is_none_or(|n| !n.is_empty() && n.len() <= 200), "contains is 1 to 200 characters");
        let root = policy.check_read(&a.path)?;
        ensure!(root.is_dir(), "{} isn't a folder", root.display());
        let started = Instant::now();
        let (mut found, mut scanned, mut stopped) = (Vec::new(), 0usize, None);
        let mut queue = VecDeque::from([(root.clone(), 0usize)]);
        'walk: while let Some((dir, depth)) = queue.pop_front() {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                scanned += 1;
                if scanned > MAX_ENTRIES || started.elapsed() > Duration::from_secs(15) {
                    stopped = Some("searched for as long as Orion does at once; narrow it with path, name or max_depth");
                    break 'walk;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                if !a.hidden && name.starts_with('.') {
                    continue;
                }
                let Ok(meta) = fs::symlink_metadata(e.path()) else { continue };
                let path = e.path();
                // Pseudo-filesystems, when searching from /.
                let pseudo = ["/proc", "/sys", "/dev", "/run"].iter().any(|p| path == Path::new(p));
                if meta.is_dir() && depth + 1 < a.max_depth && !pseudo {
                    queue.push_back((path.clone(), depth + 1));
                }
                let k = kind(&meta);
                if a.kind.as_deref().is_some_and(|want| want != k) || a.name.as_deref().is_some_and(|p| !name_matches(p, &name)) {
                    continue;
                }
                let hit = match &needle {
                    Some(n) if meta.is_file() && meta.len() <= 1 << 20 => match find_in(&path, n) {
                        Some(h) => Some(h),
                        None => continue,
                    },
                    Some(_) => continue,
                    None => None,
                };
                let modified = meta.modified().ok().and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| local_time(d.as_secs() as i64));
                let mut item = json!({ "path": path, "kind": k, "size": meta.is_file().then_some(meta.len()), "modified": modified });
                if let Some((line, text)) = hit {
                    item["line"] = json!(line);
                    item["text"] = json!(text);
                }
                found.push(item);
                if found.len() >= a.limit {
                    stopped = Some("reached the limit");
                    break 'walk;
                }
            }
        }
        let mut out = json!({ "path": root, "found": found.len(), "scanned": scanned, "results": found });
        if let Some(why) = stopped {
            out["stopped"] = json!(why);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Policy, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("work");
        fs::create_dir(&root).unwrap();
        let r = root.display().to_string();
        let policy = Policy::from_toml(&format!("[fs]\nread_roots = [{r:?}]\nwrite_roots = [{r:?}]\n")).unwrap();
        (dir, policy, r)
    }

    #[test]
    fn copies_moves_and_makes_folders() {
        let (_dir, p, r) = setup();
        FsMkdir.call(&p, json!({ "path": format!("{r}/a/b") })).unwrap();
        fs::write(format!("{r}/a/b/one.txt"), "one").unwrap();
        std::os::unix::fs::symlink("one.txt", format!("{r}/a/b/link")).unwrap();

        let c = FsCopy.call(&p, json!({ "from": format!("{r}/a"), "to": format!("{r}/copy") })).unwrap();
        assert_eq!(c["entries"], 4, "{c}");
        assert_eq!(fs::read_to_string(format!("{r}/copy/b/one.txt")).unwrap(), "one");
        assert!(fs::symlink_metadata(format!("{r}/copy/b/link")).unwrap().file_type().is_symlink());
        // Into an existing folder, as cp does; nothing replaced unless asked.
        FsCopy.call(&p, json!({ "from": format!("{r}/a/b/one.txt"), "to": format!("{r}/copy") })).unwrap();
        assert!(Path::new(&format!("{r}/copy/one.txt")).exists());
        assert!(FsCopy.precheck(&p, &json!({ "from": format!("{r}/a/b/one.txt"), "to": format!("{r}/copy") })).is_err());
        assert!(FsCopy.precheck(&p, &json!({ "from": format!("{r}/a/b/one.txt"), "to": format!("{r}/copy"), "overwrite": true })).is_ok());
        assert!(FsCopy.precheck(&p, &json!({ "from": format!("{r}/a"), "to": format!("{r}/a/b/inside") })).is_err());

        FsMove.call(&p, json!({ "from": format!("{r}/copy/one.txt"), "to": format!("{r}/renamed.txt") })).unwrap();
        assert!(Path::new(&format!("{r}/renamed.txt")).exists() && !Path::new(&format!("{r}/copy/one.txt")).exists());
        // Outside the writable roots, either end.
        assert!(FsCopy.precheck(&p, &json!({ "from": "/etc/hostname", "to": format!("{r}/h") })).is_err());
        assert!(FsMove.precheck(&p, &json!({ "from": format!("{r}/renamed.txt"), "to": "/tmp/x" })).is_err());
        assert!(FsMove.precheck(&p, &json!({ "from": r.clone(), "to": format!("{r}/x") })).is_err(), "a root itself");
    }

    #[test]
    fn symlinks_never_reach_outside() {
        let (dir, p, r) = setup();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("precious.txt"), "keep").unwrap();
        std::os::unix::fs::symlink(&outside, format!("{r}/door")).unwrap();
        // Through the link, nothing outside is moved, deleted or written.
        assert!(FsDelete.precheck(&p, &json!({ "path": format!("{r}/door/precious.txt") })).is_err());
        assert!(FsMove.precheck(&p, &json!({ "from": format!("{r}/door/precious.txt"), "to": format!("{r}/got") })).is_err());
        assert!(FsCopy.precheck(&p, &json!({ "from": format!("{r}/door/precious.txt"), "to": format!("{r}/got") })).is_err());
        assert!(FsMkdir.precheck(&p, &json!({ "path": format!("{r}/door/new") })).is_err());
        // Copying into the link, as if it were a folder, would land outside.
        assert!(FsCopy.precheck(&p, &json!({ "from": r.clone(), "to": format!("{r}/door") })).is_err());
        // Deleting the link deletes only the link.
        FsDelete.call(&p, json!({ "path": format!("{r}/door"), "permanent": true })).unwrap();
        assert_eq!(fs::read_to_string(outside.join("precious.txt")).unwrap(), "keep");
    }

    #[test]
    fn deletes_to_the_trash() {
        let (dir, p, r) = setup();
        let data = dir.path().join("data");
        // SAFETY: tests touching XDG_DATA_HOME run in this one test only.
        unsafe { std::env::set_var("XDG_DATA_HOME", &data) };
        fs::create_dir_all(format!("{r}/full/inner")).unwrap();
        fs::write(format!("{r}/a b.txt"), "x").unwrap();
        fs::write(format!("{r}/full/inner/f"), "y").unwrap();

        let d = FsDelete.call(&p, json!({ "path": format!("{r}/a b.txt") })).unwrap();
        let trashed = PathBuf::from(d["trashed_to"].as_str().unwrap());
        assert_eq!(fs::read_to_string(&trashed).unwrap(), "x");
        let info = fs::read_to_string(data.join("Trash/info/a b.txt.trashinfo")).unwrap();
        assert!(info.contains(&format!("Path={}", percent_encode(Path::new(&format!("{r}/a b.txt"))))), "{info}");
        assert!(info.contains("%20") && info.contains("DeletionDate=20"), "{info}");
        // The same name again gets a new place.
        fs::write(format!("{r}/a b.txt"), "z").unwrap();
        let again = FsDelete.call(&p, json!({ "path": format!("{r}/a b.txt") })).unwrap();
        assert_ne!(again["trashed_to"], d["trashed_to"]);

        assert!(FsDelete.precheck(&p, &json!({ "path": format!("{r}/full") })).is_err(), "needs recursive");
        FsDelete.call(&p, json!({ "path": format!("{r}/full"), "recursive": true, "permanent": true })).unwrap();
        assert!(!Path::new(&format!("{r}/full")).exists());
        assert!(FsDelete.precheck(&p, &json!({ "path": r.clone(), "recursive": true })).is_err(), "a root itself");
        assert!(FsDelete.precheck(&p, &json!({ "path": "/etc/hostname" })).is_err());
    }

    #[test]
    fn finds_by_name_and_text() {
        let (_dir, p, r) = setup();
        fs::create_dir_all(format!("{r}/docs/2026")).unwrap();
        fs::create_dir_all(format!("{r}/.cache")).unwrap();
        fs::write(format!("{r}/docs/2026/Report-Q3.PDF"), "%PDF").unwrap();
        fs::write(format!("{r}/docs/notes.txt"), "shopping\nCall the dentist on Friday\n").unwrap();
        fs::write(format!("{r}/.cache/report.pdf"), "x").unwrap();

        let by_name = FsFind.call(&p, json!({ "path": r, "name": "*.pdf" })).unwrap();
        assert_eq!(by_name["found"], 1, "hidden folders skipped: {by_name}");
        let with_hidden = FsFind.call(&p, json!({ "path": r, "name": "report", "hidden": true })).unwrap();
        assert_eq!(with_hidden["found"], 2);
        let q = FsFind.call(&p, json!({ "path": r, "name": "Report-Q?.pdf" })).unwrap();
        assert_eq!(q["found"], 1);
        let text = FsFind.call(&p, json!({ "path": r, "contains": "DENTIST" })).unwrap();
        assert_eq!(text["results"][0]["line"], 2);
        assert_eq!(text["results"][0]["text"], "Call the dentist on Friday");
        let dirs = FsFind.call(&p, json!({ "path": r, "kind": "dir" })).unwrap();
        assert_eq!(dirs["found"], 2);
        let shallow = FsFind.call(&p, json!({ "path": r, "name": "*.pdf", "max_depth": 2 })).unwrap();
        assert_eq!(shallow["found"], 0);
        assert!(FsFind.call(&p, json!({ "path": "/etc" })).is_err());

        for (pat, name, want) in [("*.pdf", "a.PDF", true), ("img_20??*", "IMG_2026_x.jpg", true), ("img_20??", "img_2026x", false), ("rep", "Report", true), ("*a*b", "xaxb", true), ("?", "", false)] {
            assert_eq!(name_matches(pat, name), want, "{pat} {name}");
        }
    }
}
