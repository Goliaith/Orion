//! The safety net under package changes. Around each `pkg_install`,
//! `pkg_remove`, `update_apply` and `pkg_rollback`, Orion
//!
//! - takes a snapper snapshot before and after, where the root filesystem
//!   has a snapper configuration (`[packages] snapshots`);
//! - writes down what changed, every package's version before and after,
//!   to `$XDG_STATE_HOME/orion/packages.jsonl` (`pkg_history` reads it).
//!
//! `pkg_rollback` puts a change back: by undoing the files the snapshot
//! pair saw change (`snapper undochange`), or else by asking the package
//! manager for the old versions again (from the repositories, or pacman's
//! cache) and removing what the change added. It refuses when a later
//! change touched the same packages, so changes are put back newest first.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::{Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::exec;
use super::packages::{complaint, manager, package_name, run_as_root, tail, Manager};
use super::updates::{command, dry_run, guard_removals, Action};
use crate::policy::Policy;
use crate::tool::{parse_args, Risk, Tool};

/// Every installed version of every package: name → versions (rpm can
/// have several of one name installed, kernels for instance).
pub(super) type Versions = BTreeMap<String, BTreeSet<String>>;

pub(super) fn versions(m: Manager) -> Result<Versions> {
    let text = match m {
        Manager::Apt => exec::output("dpkg-query", &["-W", "-f", "${db:Status-Abbrev}\t${binary:Package}\t${Version}\n"])?,
        Manager::Dnf | Manager::Zypper => exec::output("rpm", &["-qa", "--qf", "ii\t%{NAME}\t%{EPOCH}:%{VERSION}-%{RELEASE}\n"])?,
        Manager::Pacman => exec::output("pacman", &["-Q"])?
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(n, v)| format!("ii\t{n}\t{v}\n"))
            .collect(),
    };
    Ok(parse_versions(&text))
}

fn parse_versions(text: &str) -> Versions {
    let mut out = Versions::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        // dpkg lists removed-but-configured packages too ("rc").
        if f.len() < 3 || !f[0].starts_with("ii") || f[1] == "gpg-pubkey" {
            continue;
        }
        let version = f[2].strip_prefix("(none):").unwrap_or(f[2]);
        out.entry(f[1].to_string()).or_default().insert(version.to_string());
    }
    out
}

/// One package a change moved: installed (`from` none), removed (`to`
/// none), or moved from one version to another.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub(super) struct Moved {
    pub name: String,
    pub from: Option<String>,
    pub to: Option<String>,
}

fn diff(before: &Versions, after: &Versions) -> Vec<Moved> {
    let none = BTreeSet::new();
    let names: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    let mut out = Vec::new();
    for name in names {
        let (b, a) = (before.get(name).unwrap_or(&none), after.get(name).unwrap_or(&none));
        let gone: Vec<&String> = b.difference(a).collect();
        let new: Vec<&String> = a.difference(b).collect();
        if let ([from], [to]) = (gone.as_slice(), new.as_slice()) {
            out.push(Moved { name: name.clone(), from: Some((*from).clone()), to: Some((*to).clone()) });
            continue;
        }
        out.extend(gone.into_iter().map(|v| Moved { name: name.clone(), from: Some(v.clone()), to: None }));
        out.extend(new.into_iter().map(|v| Moved { name: name.clone(), from: None, to: Some(v.clone()) }));
    }
    out
}

/// A snapper pre/post pair around a change.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub(super) struct Snapshot {
    pub config: String,
    pub pre: u32,
    pub post: Option<u32>,
}

/// One change to the installed packages, as `packages.jsonl` keeps it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(super) struct Change {
    pub id: u64,
    pub at: String,
    pub tool: String,
    pub manager: String,
    pub ok: bool,
    pub packages: Vec<Moved>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Snapshot>,
    /// For a rollback: the change it put back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undoes: Option<u64>,
}

/// The tools whose changes are tracked, which name the snapshots.
const TRACKED: [&str; 4] = ["update_apply", "pkg_install", "pkg_remove", "pkg_rollback"];

fn history_path() -> Option<PathBuf> {
    Policy::state_dir().map(|d| d.join("packages.jsonl"))
}

fn parse_history(text: &str) -> Vec<Change> {
    text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

pub(super) fn history() -> Result<Vec<Change>> {
    let path = history_path().context("no state directory for the package history (is HOME set?)")?;
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(parse_history(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Append `change` with the next id, under a lock: the daemon and a chat
/// may both write.
fn record(path: &Path, mut change: Change) -> Result<u64> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut f = OpenOptions::new().create(true).read(true).append(true).open(path).with_context(|| format!("opening {}", path.display()))?;
    // SAFETY: flock on a file descriptor we own; released when `f` closes.
    ensure!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0, "locking {}", path.display());
    let mut text = String::new();
    f.rewind()?;
    f.read_to_string(&mut text)?;
    change.id = parse_history(&text).iter().map(|c| c.id).max().unwrap_or(0) + 1;
    let mut line = serde_json::to_string(&change)?;
    line.push('\n');
    f.write_all(line.as_bytes())?;
    Ok(change.id)
}

fn now() -> String {
    let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    super::journal_time(secs)
}

fn valid_config(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
}

/// The snapper configuration to snapshot with, or why there is none.
pub(super) fn snapper(policy: &Policy) -> Result<String, String> {
    let p = &policy.packages;
    let config = p.snapper_config.as_str();
    match p.snapshots.as_str() {
        "never" => return Err("packages.snapshots is \"never\"".into()),
        "auto" | "required" => {}
        other => return Err(format!("packages.snapshots must be auto, required or never, not {other:?}")),
    }
    if !valid_config(config) {
        return Err(format!("packages.snapper_config {config:?} isn't a snapper configuration name"));
    }
    if !exec::installed("snapper") {
        return Err("snapper isn't installed".into());
    }
    if !Path::new("/etc/snapper/configs").join(config).is_file() {
        return Err(format!("snapper has no {config:?} configuration (snapper -c {config} create-config /)"));
    }
    Ok(config.to_string())
}

enum Snap<'a> {
    Pre(&'a str),
    Post(u32, &'a str),
    Undo(u32, u32),
}

fn snapper_args(config: &str, op: Snap) -> Vec<String> {
    let mut v: Vec<String> = ["--no-dbus", "-c", config].map(String::from).to_vec();
    let s = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    match op {
        Snap::Pre(tool) => v.extend(s(&["create", "--type", "pre", "--print-number", "--cleanup-algorithm", "number", "--description"])
            .into_iter()
            .chain([format!("orion {tool}")])),
        Snap::Post(pre, tool) => v.extend(
            s(&["create", "--type", "post", "--pre-number"])
                .into_iter()
                .chain([pre.to_string()])
                .chain(s(&["--print-number", "--cleanup-algorithm", "number", "--description"]))
                .chain([format!("orion {tool}")]),
        ),
        Snap::Undo(pre, post) => v.extend(["undochange".to_string(), format!("{pre}..{post}")]),
    }
    v
}

fn run_snapper(policy: &Policy, args: &[String]) -> Result<exec::Finished> {
    let how = exec::elevation(policy)?;
    let done = exec::run_as(how.as_ref(), "snapper", args, None, Duration::from_secs(600), 256 << 10)?;
    ensure!(done.success(), "snapper: {}", complaint(&done));
    Ok(done)
}

fn take(policy: &Policy, config: &str, op: Snap) -> Result<u32> {
    let done = run_snapper(policy, &snapper_args(config, op))?;
    done.stdout.trim().parse().with_context(|| format!("snapper printed {:?}, not a snapshot number", done.stdout.trim()))
}

/// The privileged helper's check of a snapper command: exactly one Orion
/// builds, on the helper's own snapper configuration.
pub(super) fn helper_check(policy: &Policy, args: &[String]) -> Result<()> {
    let config = snapper(policy).map_err(|why| anyhow::anyhow!("the helper takes no snapshots: {why}"))?;
    let number = |i: usize| args.get(i).and_then(|a| a.parse::<u32>().ok().filter(|_| a.chars().all(|c| c.is_ascii_digit())));
    let tool = args.last().and_then(|d| d.strip_prefix("orion ")).and_then(|t| TRACKED.iter().find(|x| **x == t)).copied();
    let undo = args.get(4).and_then(|r| r.split_once("..")).and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)));
    let candidates = [
        tool.map(|t| snapper_args(&config, Snap::Pre(t))),
        tool.zip(number(7)).map(|(t, n)| snapper_args(&config, Snap::Post(n, t))),
        undo.map(|(a, b)| snapper_args(&config, Snap::Undo(a, b))),
    ];
    ensure!(candidates.iter().flatten().any(|c| c == args), "that isn't a snapper command Orion makes for the {config:?} configuration");
    Ok(())
}

/// Refuse a change up front when a snapshot is required and impossible.
pub(super) fn precheck(policy: &Policy) -> Result<()> {
    if policy.packages.snapshots == "required" {
        snapper(policy).map_err(|why| anyhow::anyhow!("packages.snapshots is \"required\", but {why}"))?;
    }
    Ok(())
}

/// A change under way: the versions and snapshot from before it.
pub(super) struct Tracker {
    m: Manager,
    tool: &'static str,
    before: Option<Versions>,
    snapshot: Option<Snapshot>,
    /// Why no snapshot was taken.
    skipped: Option<String>,
    undoes: Option<u64>,
}

/// Get ready for a change: note every version and take the "pre" snapshot.
pub(super) fn begin(policy: &Policy, m: Manager, tool: &'static str, undoes: Option<u64>) -> Result<Tracker> {
    let required = policy.packages.snapshots == "required";
    let (snapshot, skipped) = match snapper(policy) {
        Ok(config) => match take(policy, &config, Snap::Pre(tool)) {
            Ok(pre) => (Some(Snapshot { config, pre, post: None }), None),
            Err(e) if required => bail!("packages.snapshots is \"required\", and the snapshot failed: {e:#}"),
            Err(e) => (None, Some(format!("{e:#}"))),
        },
        Err(why) if required => bail!("packages.snapshots is \"required\", but {why}"),
        Err(why) => (None, Some(why)),
    };
    let before = versions(m).map_err(|e| tracing::warn!("package history: {e:#}")).ok();
    Ok(Tracker { m, tool, before, snapshot, skipped, undoes })
}

impl Tracker {
    /// After the change: the "post" snapshot, and the record of what moved,
    /// noted in `out` (`change`, `snapshot`).
    pub(super) fn finish(mut self, policy: &Policy, ok: bool, out: &mut Value) -> Option<Change> {
        if let Some(s) = &mut self.snapshot {
            match take(policy, &s.config, Snap::Post(s.pre, self.tool)) {
                Ok(n) => s.post = Some(n),
                Err(e) => tracing::warn!("snapper post snapshot: {e:#}"),
            }
        }
        match &self.snapshot {
            Some(s) => out["snapshot"] = json!(s),
            None => out["snapshot"] = json!({ "skipped": self.skipped }),
        }
        let after = versions(self.m).map_err(|e| tracing::warn!("package history: {e:#}")).ok();
        let packages = match (&self.before, &after) {
            (Some(b), Some(a)) => diff(b, a),
            _ => Vec::new(),
        };
        if packages.is_empty() && self.snapshot.is_none() {
            return None;
        }
        let mut change = Change {
            id: 0,
            at: now(),
            tool: self.tool.into(),
            manager: self.m.name().into(),
            ok,
            packages,
            snapshot: self.snapshot,
            undoes: self.undoes,
        };
        match history_path().map(|p| record(&p, change.clone())) {
            Some(Ok(id)) => {
                change.id = id;
                out["change"] = json!(id);
                if self.tool != "pkg_rollback" {
                    out["undo"] = json!(format!("pkg_rollback with id {id} puts this change back"));
                }
                Some(change)
            }
            Some(Err(e)) => {
                tracing::warn!("package history: {e:#}");
                None
            }
            None => None,
        }
    }
}

/// The change `id`, checked for putting back: not a rollback, not put back
/// already, and no later change that touched its packages still in place.
fn rollback_target(changes: &[Change], id: u64) -> Result<Change> {
    let target = changes.iter().find(|c| c.id == id).with_context(|| format!("there is no change {id} in the package history"))?;
    ensure!(target.undoes.is_none(), "change {id} is itself a rollback (of change {}); install or update to go forward again", target.undoes.unwrap_or(0));
    if let Some(r) = changes.iter().find(|c| c.undoes == Some(id) && c.ok) {
        bail!("change {id} was already put back, by change {}", r.id);
    }
    let names: BTreeSet<&str> = target.packages.iter().map(|p| p.name.as_str()).collect();
    let undone: BTreeSet<u64> = changes.iter().filter(|c| c.ok).filter_map(|c| c.undoes).collect();
    let later = changes.iter().filter(|c| c.id > id && !undone.contains(&c.id) && c.undoes.is_none_or(|u| u <= id));
    for c in later {
        if let Some(p) = c.packages.iter().find(|p| names.contains(p.name.as_str())) {
            bail!("change {} (from {}) changed {} since; put that back first (pkg_rollback with id {})", c.id, c.at, p.name, c.id);
        }
    }
    Ok(target.clone())
}

/// An old version to install again: the package, its version, and how the
/// package manager names it (`name=version`, dnf `name-version`, or
/// pacman's cached package file).
#[derive(Debug, PartialEq)]
struct Old {
    name: String,
    version: String,
    spec: String,
}

/// How to put a change back with the package manager.
#[derive(Debug, Default, PartialEq)]
struct Restore {
    install: Vec<Old>,
    /// Packages the change added, to remove.
    remove: Vec<String>,
    /// What stays as it is, and why.
    left: Vec<String>,
}

const PACMAN_CACHE: &str = "/var/cache/pacman/pkg";

/// pacman's cached package file for `name` at `version`, if it is there.
fn pacman_cached(dir: &Path, name: &str, version: &str) -> Option<String> {
    let prefix = format!("{name}-{version}-");
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|f| f.strip_prefix(&prefix).is_some_and(|rest| !rest.contains('-') && rest.contains(".pkg.tar") && !rest.ends_with(".sig")))
        .collect();
    found.sort();
    found.pop().map(|f| dir.join(f).display().to_string())
}

fn restore_plan(policy: &Policy, m: Manager, change: &Change, cache: &Path) -> Result<Restore> {
    let mut r = Restore::default();
    let protected = |name: &str| policy.packages.protected.iter().any(|p| exec::glob_match(p, name.split(':').next().unwrap_or(name)));
    let now = versions(m)?;
    for p in &change.packages {
        match (&p.from, &p.to) {
            (Some(from), _) => {
                if now.get(&p.name).is_some_and(|v| v.contains(from)) {
                    continue; // already back
                }
                let spec = match m {
                    Manager::Apt | Manager::Zypper => format!("{}={from}", p.name),
                    Manager::Dnf => format!("{}-{from}", p.name),
                    Manager::Pacman => pacman_cached(cache, &p.name, from)
                        .with_context(|| format!("{} {from} isn't in pacman's cache ({}) any more", p.name, cache.display()))?,
                };
                r.install.push(Old { name: p.name.clone(), version: from.clone(), spec });
            }
            (None, Some(to)) => {
                let installed = now.get(&p.name);
                if !installed.is_some_and(|v| v.contains(to)) {
                    continue; // gone already
                }
                if protected(&p.name) {
                    r.left.push(format!("{} {to}: protected, so it stays installed", p.name));
                } else if installed.is_some_and(|v| v.len() > 1) {
                    r.left.push(format!("{} {to}: other versions are installed too, so it stays", p.name));
                } else {
                    r.remove.push(p.name.clone());
                }
            }
            (None, None) => {}
        }
    }
    ensure!(!r.install.is_empty() || !r.remove.is_empty(), "change {} has nothing left to put back", change.id);
    Ok(r)
}

/// A package to install again, as [`restore_plan`] writes it: checked by
/// the privileged helper too.
pub(super) fn restore_spec(m: Manager, raw: &str) -> Result<()> {
    let version_ok = |v: &str| {
        !v.is_empty() && v.len() <= 200 && v.starts_with(|c: char| c.is_ascii_alphanumeric()) && v.chars().all(|c| c.is_ascii_alphanumeric() || ".+~:_-".contains(c))
    };
    match m {
        Manager::Apt | Manager::Zypper => {
            let (name, version) = raw.split_once('=').with_context(|| format!("{raw:?} isn't name=version"))?;
            package_name(name)?;
            ensure!(version_ok(version), "{raw:?} isn't name=version");
        }
        Manager::Dnf => {
            package_name(raw)?;
        }
        Manager::Pacman => {
            let file = raw.strip_prefix(PACMAN_CACHE).and_then(|r| r.strip_prefix('/')).with_context(|| format!("{raw:?} isn't in {PACMAN_CACHE}"))?;
            let ok = !file.contains('/')
                && !file.starts_with('.')
                && file.contains(".pkg.tar")
                && file.chars().all(|c| c.is_ascii_alphanumeric() || "+-._:@".contains(c));
            ensure!(ok, "{raw:?} isn't a package file in {PACMAN_CACHE}");
            let meta = std::fs::symlink_metadata(raw).with_context(|| format!("{raw:?}"))?;
            ensure!(meta.file_type().is_file(), "{raw:?} isn't a plain file");
        }
    }
    Ok(())
}

impl Restore {
    fn specs(&self) -> Vec<String> {
        self.install.iter().map(|o| o.spec.clone()).collect()
    }
}

/// Check removing what the change added, by the dry run: nothing else
/// may go with it.
fn check_removal(policy: &Policy, m: Manager, r: &Restore) -> Result<()> {
    let plan = dry_run(policy, m, Action::Remove, &r.remove)?;
    guard_removals(policy, m, &plan, true)?;
    let ours: BTreeSet<&str> = r.remove.iter().map(String::as_str).collect();
    let extra: Vec<&str> = plan.remove.iter().map(|(n, _)| n.as_str()).filter(|n| !ours.contains(n.split(':').next().unwrap_or(n))).collect();
    ensure!(extra.is_empty(), "removing {} would also remove {}", r.remove.join(", "), extra.join(", "));
    Ok(())
}

/// Check that the package manager can put the change back, by its dry
/// runs, and in which order: the old versions first, then removing what
/// the change added (when it brought in a new dependency, say); or the
/// additions out first when the old versions can't go in beside them (a
/// package that replaced another).
fn check_restore(policy: &Policy, m: Manager, r: &Restore) -> Result<bool> {
    if r.install.is_empty() {
        check_removal(policy, m, r)?;
        return Ok(true);
    }
    let install = dry_run(policy, m, Action::Restore, &r.specs()).and_then(|plan| guard_removals(policy, m, &plan, true));
    match install {
        Ok(()) => Ok(true),
        Err(e) if !r.remove.is_empty() && check_removal(policy, m, r).is_ok() => {
            tracing::info!("rollback: the old versions go in after the removal ({e:#})");
            Ok(false)
        }
        Err(e) => Err(e.context("the old versions can't be installed")),
    }
}

/// The old versions that aren't installed (yet).
fn missing(m: Manager, r: &Restore) -> Vec<String> {
    let now = versions(m).unwrap_or_default();
    r.install.iter().filter(|o| !now.get(&o.name).is_some_and(|v| v.contains(&o.version))).map(|o| o.spec.clone()).collect()
}

/// Run the rollback's package manager steps, noting each in `out`.
fn restore(policy: &Policy, m: Manager, r: &Restore, install_first: bool, out: &mut Value) -> Result<bool> {
    let step = |action: Action, list: Vec<String>, key: &str, out: &mut Value| -> Result<bool> {
        if list.is_empty() {
            return Ok(true);
        }
        let (done, elevated) = run_as_root(policy, m, command(m, action, &list, false))?;
        out["elevated"] = json!(elevated);
        if !done.success() {
            out["error"] = json!(complaint(&done));
            return Ok(false);
        }
        let mut have: Vec<Value> = out[key].as_array().cloned().unwrap_or_default();
        have.extend(list.into_iter().map(Value::from));
        out[key] = json!(have);
        Ok(true)
    };
    if install_first && !step(Action::Restore, r.specs(), "reinstalled", out)? {
        if r.remove.is_empty() {
            return Ok(false);
        }
        // They wouldn't go in beside what the change added: after it, then.
        tracing::info!("rollback: the old versions go in after the removal ({})", out["error"]);
        out["error"] = Value::Null;
    }
    if !r.remove.is_empty() {
        // Removing what the change added must take nothing else with it.
        if let Err(e) = check_removal(policy, m, r) {
            out["error"] = json!(format!("{e:#}"));
            return Ok(false);
        }
        if !step(Action::Remove, r.remove.clone(), "removed", out)? {
            return Ok(false);
        }
    }
    // Anything the first round couldn't put back beside what was there.
    let ok = step(Action::Restore, missing(m, r), "reinstalled", out)?;
    if let Some(o) = out.as_object_mut().filter(|_| ok) {
        o.remove("error");
    }
    Ok(ok)
}

pub struct PkgHistory;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryArgs {
    #[serde(default = "ten")]
    limit: usize,
}

fn ten() -> usize {
    10
}

impl Tool for PkgHistory {
    fn name(&self) -> &'static str {
        "pkg_history"
    }

    fn description(&self) -> &'static str {
        "The package changes Orion made (installs, removals, updates, rollbacks), newest first: each package's \
         version before and after, the snapper snapshots taken around it, and whether it was put back since. \
         Use the id with pkg_rollback."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 10, "description": "How many changes." }
            },
            "additionalProperties": false
        })
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: HistoryArgs = parse_args(args)?;
        let all = history()?;
        let shown: Vec<Value> = all
            .iter()
            .rev()
            .take(a.limit.clamp(1, 100))
            .map(|c| {
                let mut v = json!(c);
                v["packages"] = json!(c.packages.iter().take(50).collect::<Vec<_>>());
                if c.packages.len() > 50 {
                    v["more_packages"] = json!(c.packages.len() - 50);
                }
                if let Some(r) = all.iter().find(|r| r.undoes == Some(c.id) && r.ok) {
                    v["rolled_back_by"] = json!(r.id);
                }
                v
            })
            .collect();
        Ok(json!({
            "snapshots": match snapper(policy) {
                Ok(config) => json!({ "snapper": config }),
                Err(why) => json!({ "off": why }),
            },
            "count": all.len(),
            "changes": shown,
        }))
    }
}

pub struct PkgRollback;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RollbackArgs {
    id: u64,
    #[serde(default = "auto")]
    method: String,
}

fn auto() -> String {
    "auto".into()
}

/// How a rollback will go, worked out before any approval.
enum Plan {
    Snapshot(Snapshot),
    /// And whether the old versions go in first.
    Packages(Restore, bool),
}

fn plan(policy: &Policy, args: &Value) -> Result<(Manager, Change, Plan)> {
    let a: RollbackArgs = parse_args(args.clone())?;
    let m = manager(policy)?;
    let change = rollback_target(&history()?, a.id)?;
    ensure!(change.manager == m.name(), "change {} was made with {}, and this machine uses {}", a.id, change.manager, m.name());
    let pair = change.snapshot.clone().filter(|s| s.post.is_some());
    let by_snapshot = |s: Snapshot| -> Result<Plan> {
        let config = snapper(policy).map_err(|why| anyhow::anyhow!("can't use the snapshots: {why}"))?;
        ensure!(config == s.config, "change {} was snapshotted with snapper config {:?}; packages.snapper_config is {config:?}", a.id, s.config);
        Ok(Plan::Snapshot(s))
    };
    let p = match (a.method.as_str(), pair) {
        ("snapshot", Some(s)) => by_snapshot(s)?,
        ("snapshot", None) => bail!("change {} has no snapshot pair to go back to; use method \"packages\"", a.id),
        ("auto", Some(s)) if snapper(policy).is_ok() => by_snapshot(s)?,
        ("auto" | "packages", _) => {
            let r = restore_plan(policy, m, &change, Path::new(PACMAN_CACHE))?;
            let install_first = check_restore(policy, m, &r)?;
            Plan::Packages(r, install_first)
        }
        (other, _) => bail!("method must be auto, snapshot or packages, not {other:?}"),
    };
    Ok((m, change, p))
}

impl Tool for PkgRollback {
    fn name(&self) -> &'static str {
        "pkg_rollback"
    }

    fn description(&self) -> &'static str {
        "Put back a package change Orion made (an id from pkg_history), as root. With a snapper snapshot pair \
         around the change, undoes every file it changed (method snapshot, the default when there is one); \
         otherwise installs the old versions again, from the repositories or pacman's cache, and removes what \
         the change added (method packages). Changes are put back newest first. Protected packages stay. \
         A reboot is wise afterwards when the kernel or core libraries were involved."
    }

    fn risk(&self) -> Risk {
        Risk::Execute
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "minimum": 1, "description": "The change to put back, from pkg_history." },
                "method": { "type": "string", "enum": ["auto", "snapshot", "packages"], "default": "auto",
                            "description": "snapshot: snapper undochange; packages: the package manager. auto: the snapshot when there is one." }
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        precheck(policy)?;
        plan(policy, args).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let (m, change, p) = plan(policy, &args)?;
        let tracker = begin(policy, m, "pkg_rollback", Some(change.id))?;
        let mut out = json!({ "manager": m.name(), "undoes": change.id });
        let ok = match p {
            Plan::Snapshot(s) => {
                out["method"] = json!("snapshot");
                let post = s.post.unwrap_or(s.pre);
                match run_snapper(policy, &snapper_args(&s.config, Snap::Undo(s.pre, post))) {
                    Ok(done) => {
                        out["output"] = json!(tail(&done.stdout));
                        // The pair must have held the package database too.
                        let now = versions(m).unwrap_or_default();
                        let back = |p: &Moved| match &p.from {
                            Some(from) => now.get(&p.name).is_some_and(|v| v.contains(from)),
                            None => !p.to.as_ref().is_some_and(|to| now.get(&p.name).is_some_and(|v| v.contains(to))),
                        };
                        let covered = change.packages.is_empty() || change.packages.iter().any(back);
                        if !covered {
                            out["error"] = json!(format!(
                                "snapper undid the files of {:?}, but none of the packages went back: that configuration doesn't hold \
                                 the package database; use method \"packages\"",
                                s.config
                            ));
                        }
                        covered
                    }
                    Err(e) => {
                        out["error"] = json!(format!("{e:#}"));
                        false
                    }
                }
            }
            Plan::Packages(r, install_first) => {
                out["method"] = json!("packages");
                out["left"] = json!(r.left);
                let ok = restore(policy, m, &r, install_first, &mut out)?;
                let still = missing(m, &r);
                if ok && !still.is_empty() {
                    out["error"] = json!(format!("{} didn't go back in", still.join(", ")));
                }
                ok && still.is_empty()
            }
        };
        out["ok"] = json!(ok);
        if let Some(c) = tracker.finish(policy, ok, &mut out) {
            out["now"] = json!(c.packages);
        }
        if ok {
            out["message"] = json!(format!("change {} is put back", change.id));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(id: u64, packages: &[(&str, Option<&str>, Option<&str>)], undoes: Option<u64>) -> Change {
        Change {
            id,
            at: format!("2026-10-{id:02} 04:00:00"),
            tool: if undoes.is_some() { "pkg_rollback" } else { "update_apply" }.into(),
            manager: "apt".into(),
            ok: true,
            packages: packages
                .iter()
                .map(|(n, f, t)| Moved { name: n.to_string(), from: f.map(String::from), to: t.map(String::from) })
                .collect(),
            snapshot: None,
            undoes,
        }
    }

    #[test]
    fn notes_what_moved() {
        let before = parse_versions(
            "ii \tcurl\t8.5.0-2\nii \tlibc6:i386\t2.39-0\nrc \told\t1.0\nii\tkernel-core\t(none):6.1-1\nii\tgpg-pubkey\tx\nii\tgone\t1.0\n",
        );
        assert_eq!(before["kernel-core"].iter().next().unwrap(), "6.1-1");
        assert!(!before.contains_key("old") && !before.contains_key("gpg-pubkey"));
        let after = parse_versions("ii\tcurl\t8.5.0-3\nii\tlibc6:i386\t2.39-0\nii\tkernel-core\t6.1-1\nii\tkernel-core\t6.2-1\nii\tnew\t1:2.0\n");
        let moved = diff(&before, &after);
        let m = |n: &str, f: Option<&str>, t: Option<&str>| Moved { name: n.into(), from: f.map(String::from), to: t.map(String::from) };
        assert_eq!(
            moved,
            [m("curl", Some("8.5.0-2"), Some("8.5.0-3")), m("gone", Some("1.0"), None), m("kernel-core", None, Some("6.2-1")), m("new", None, Some("1:2.0"))]
        );
    }

    #[test]
    fn keeps_a_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orion/packages.jsonl");
        assert_eq!(record(&path, change(0, &[("curl", Some("1"), Some("2"))], None)).unwrap(), 1);
        assert_eq!(record(&path, change(0, &[("curl", Some("2"), Some("1"))], Some(1))).unwrap(), 2);
        let all = parse_history(&std::fs::read_to_string(&path).unwrap());
        assert_eq!((all.len(), all[1].id, all[1].undoes), (2, 2, Some(1)));
    }

    #[test]
    fn puts_changes_back_newest_first() {
        let h = vec![
            change(1, &[("curl", Some("1"), Some("2"))], None),
            change(2, &[("vim", None, Some("9"))], None),
            change(3, &[("curl", Some("2"), Some("3")), ("zsh", Some("5"), Some("6"))], None),
        ];
        let err = |h: &[Change], id| format!("{:#}", rollback_target(h, id).unwrap_err());
        assert!(err(&h, 1).contains("change 3"), "{}", err(&h, 1));
        assert_eq!(rollback_target(&h, 2).unwrap().id, 2); // nothing later touched vim
        assert!(err(&h, 9).contains("no change 9"));
        // Once 3 is put back, 1 may be too; and neither 3 nor its rollback again.
        let mut h = h;
        h.push(change(4, &[("curl", Some("3"), Some("2")), ("zsh", Some("6"), Some("5"))], Some(3)));
        assert_eq!(rollback_target(&h, 1).unwrap().id, 1);
        assert!(err(&h, 3).contains("already put back, by change 4"));
        assert!(err(&h, 4).contains("itself a rollback"));
    }

    #[test]
    fn plans_the_way_back() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("orion-demo-1.0-1-any.pkg.tar.zst"), "").unwrap();
        std::fs::write(dir.path().join("orion-demo-1.0-1-any.pkg.tar.zst.sig"), "").unwrap();
        std::fs::write(dir.path().join("orion-demo-extra-1.0-1-any.pkg.tar.zst"), "").unwrap();
        assert_eq!(pacman_cached(dir.path(), "orion-demo", "1.0-1").unwrap(), dir.path().join("orion-demo-1.0-1-any.pkg.tar.zst").display().to_string());
        assert_eq!(pacman_cached(dir.path(), "orion-demo", "2.0-1"), None);

        for ok in ["curl=8.5.0-2ubuntu10.15", "libc6:i386=2.39-0ubuntu8~1", "kernel-default=6.1.2-1.1"] {
            restore_spec(Manager::Apt, ok).unwrap();
        }
        for bad in ["curl", "curl=-y", "-o=1", "curl=1 2", "curl=1;id"] {
            assert!(restore_spec(Manager::Apt, bad).is_err(), "{bad}");
        }
        restore_spec(Manager::Dnf, "orion-demo-1.0-1").unwrap();
        assert!(restore_spec(Manager::Dnf, "--setopt=x").is_err());
        for bad in ["/tmp/x.pkg.tar.zst", "/var/cache/pacman/pkg/../../../etc/shadow", "/var/cache/pacman/pkg/sub/x.pkg.tar.zst", "/var/cache/pacman/pkg/x.txt"] {
            assert!(restore_spec(Manager::Pacman, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn snapper_commands() {
        let p = Policy::from_toml("[packages]\nsnapshots = \"never\"\n").unwrap();
        assert!(snapper(&p).unwrap_err().contains("never"));
        let p = Policy::from_toml("[packages]\nsnapper_config = \"../x\"\n").unwrap();
        assert!(snapper(&p).unwrap_err().contains("isn't a snapper configuration name"));
        assert_eq!(
            snapper_args("root", Snap::Post(41, "update_apply")).join(" "),
            "--no-dbus -c root create --type post --pre-number 41 --print-number --cleanup-algorithm number --description orion update_apply"
        );
        assert_eq!(snapper_args("root", Snap::Undo(41, 42)).join(" "), "--no-dbus -c root undochange 41..42");
        // A required snapshot that can't be taken stops the change before approval.
        let p = Policy::from_toml("[packages]\nsnapshots = \"required\"\nsnapper_config = \"orion-no-such\"\n").unwrap();
        assert!(format!("{:#}", precheck(&p).unwrap_err()).contains("required"));
    }
}
