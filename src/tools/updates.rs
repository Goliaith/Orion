//! System updates: what can be upgraded (and which of it fixes security
//! problems), upgrading, and what still runs old code afterwards: the
//! services to restart and whether the machine needs a reboot.
//!
//! Like `pkg_install`, upgrades run as root and outside the sandbox. For apt
//! the upgrade is simulated first, and refused before any approval if it
//! would remove a package the system needs or the policy protects.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, ensure, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::exec::{self, glob_match};
use super::packages::{complaint, essential, manager, package_name, run_as_root, tail, Manager};
use crate::policy::Policy;
use crate::tool::{parse_args, Preapproval, Risk, Tool};

/// One package that can be upgraded.
#[derive(Debug, Default, PartialEq)]
struct Update {
    name: String,
    from: Option<String>,
    to: String,
    /// The repositories (suites) the new version comes from.
    origin: String,
    security: bool,
}

impl Update {
    fn json(&self) -> Value {
        json!({
            "name": self.name, "from": self.from, "to": self.to, "origin": self.origin,
            "security": self.security, "kernel": kernel_package(&self.name),
        })
    }
}

fn kernel_package(name: &str) -> bool {
    ["linux", "linux-lts", "linux-zen", "linux-hardened", "kernel", "kernel-core"].contains(&name)
        || ["linux-image-", "linux-modules-", "kernel-core-", "kernel-modules"].iter().any(|p| name.starts_with(p))
}

fn security_origin(origin: &str) -> bool {
    origin.to_ascii_lowercase().contains("security")
}

/// `apt list --upgradable`:
/// `curl/noble-updates,noble-security 8.5.0-2ubuntu10.16 amd64 [upgradable from: 8.5.0-2ubuntu10.15]`
fn parse_apt_upgradable(text: &str) -> Vec<Update> {
    text.lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once('/')?;
            let mut f = rest.split_whitespace();
            let (origin, to) = (f.next()?, f.next()?);
            let from = line.split_once("[upgradable from: ").map(|(_, v)| v.trim_end_matches(']').to_string());
            Some(Update { name: name.into(), from, to: to.into(), origin: origin.into(), security: security_origin(origin) })
        })
        .collect()
}

/// What `apt-get -s upgrade` (or `dist-upgrade`) would do.
#[derive(Debug, Default)]
struct UpgradePlan {
    upgrade: Vec<Update>,
    /// New packages pulled in (a new kernel, a renamed library).
    install: Vec<(String, String)>,
    remove: Vec<(String, String)>,
    /// Upgradable, but left alone: held, phased, or needing a removal.
    kept_back: Vec<String>,
}

fn parse_apt_upgrade(text: &str) -> UpgradePlan {
    let mut plan = UpgradePlan::default();
    let mut in_kept = false;
    for line in text.lines() {
        if line.starts_with("The following packages have been kept back")
            || line.starts_with("The following upgrades have been deferred")
        {
            in_kept = true;
            continue;
        }
        if in_kept && line.starts_with(' ') {
            plan.kept_back.extend(line.split_whitespace().map(String::from));
            continue;
        }
        in_kept = false;
        // "Inst curl [8.5.0-2ubuntu10.15] (8.5.0-2ubuntu10.16 Ubuntu:24.04/noble-security [amd64])"
        // "Inst linux-image-6.8.0-50-generic (6.8.0-50.51 Ubuntu:24.04/noble-updates [amd64])"
        // "Remv foo [1.0]"
        let (verb, rest) = line.split_once(' ').unwrap_or_default();
        let name = rest.split_whitespace().next().unwrap_or_default().to_string();
        let old = rest.split_once(" [").and_then(|(_, r)| r.split_once(']')).map(|(v, _)| v.to_string());
        let paren = rest.split_once(" (").and_then(|(_, r)| r.rsplit_once(')')).map(|(v, _)| v);
        match (verb, paren) {
            ("Inst", Some(p)) => {
                let (to, sources) = p.split_once(' ').unwrap_or((p, ""));
                // "Ubuntu:24.04/noble-updates, Ubuntu:24.04/noble-security [all]"
                let origin = sources
                    .split(", ")
                    .map(|s| s.split(" [").next().unwrap_or(s))
                    .map(|s| s.rsplit_once('/').map_or(s, |(_, suite)| suite))
                    .collect::<Vec<_>>()
                    .join(",");
                // A version in brackets before the parenthesis is the one installed now.
                let old = old.filter(|_| rest.find(" [") < rest.find(" ("));
                match old {
                    Some(from) => plan.upgrade.push(Update {
                        name,
                        from: Some(from),
                        to: to.into(),
                        security: security_origin(&origin),
                        origin,
                    }),
                    None => plan.install.push((name, to.into())),
                }
            }
            ("Remv", _) => plan.remove.push((name, old.unwrap_or_default())),
            _ => {}
        }
    }
    plan
}

/// `dnf check-update`: `curl.x86_64   8.6.0-1.fc40   updates`, after a
/// header and before any "Obsoleting Packages" section.
fn parse_dnf_updates(text: &str, security: &BTreeSet<String>) -> Vec<Update> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with("Obsoleting") {
            break;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        let [nevra, to, repo] = f[..] else { continue };
        let Some((name, _arch)) = nevra.rsplit_once('.') else { continue };
        if !to.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        out.push(Update {
            name: name.into(),
            from: None,
            to: to.into(),
            origin: repo.into(),
            security: security.contains(name),
        });
    }
    out
}

/// `dnf updateinfo list --security --updates`:
/// `FEDORA-2024-1a2b3c  Important/Sec.  curl-8.6.0-1.fc40.x86_64`
fn parse_dnf_security(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|l| l.split_whitespace().nth(2))
        .filter_map(|nevra| {
            let no_arch = nevra.rsplit_once('.')?.0;
            let (rest, _release) = no_arch.rsplit_once('-')?;
            Some(rest.rsplit_once('-')?.0.to_string())
        })
        .collect()
}

/// `checkupdates` / `pacman -Qu`: `curl 8.5.0-1 -> 8.6.0-1`.
fn parse_pacman_updates(text: &str) -> Vec<Update> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            match f[..] {
                [name, from, "->", to, ..] => {
                    Some(Update { name: name.into(), from: Some(from.into()), to: to.into(), ..Default::default() })
                }
                _ => None,
            }
        })
        .collect()
}

fn run_quiet(program: &str, args: &[&str], secs: u64) -> Result<exec::Finished> {
    let mut cmd = Command::new(program);
    cmd.args(args).env("LC_ALL", "C");
    exec::run(cmd, None, Duration::from_secs(secs), 4 << 20)
}

/// Fetch the latest package lists. Returns why it couldn't, if it couldn't.
fn refresh(policy: &Policy, m: Manager) -> Option<String> {
    let argv: Vec<String> = match m {
        Manager::Apt => ["DEBIAN_FRONTEND=noninteractive", "apt-get", "update", "-q"].map(String::from).to_vec(),
        Manager::Dnf => ["dnf", "makecache", "-q"].map(String::from).to_vec(),
        // `pacman -Sy` alone would set up a partial upgrade; checkupdates
        // refreshes a private copy of the database instead.
        Manager::Pacman => return None,
    };
    match run_as_root(policy, m, argv) {
        Ok((done, _)) if done.success() => None,
        Ok((done, _)) => Some(complaint(&done)),
        Err(e) => Some(format!("{e:#}")),
    }
}

fn pending(m: Manager) -> Result<Vec<Update>> {
    Ok(match m {
        Manager::Apt => parse_apt_upgradable(&exec::output("apt", &["list", "--upgradable"])?),
        Manager::Dnf => {
            // check-update exits 100 when there are updates.
            let done = run_quiet("dnf", &["-q", "check-update"], 300)?;
            ensure!(done.success() || done.status.code() == Some(100), "dnf check-update: {}", done.complaint());
            let security = run_quiet("dnf", &["-q", "updateinfo", "list", "--security", "--updates"], 300)
                .map(|d| parse_dnf_security(&d.stdout))
                .unwrap_or_default();
            let mut list = parse_dnf_updates(&done.stdout, &security);
            let names: Vec<&str> = list.iter().map(|u| u.name.as_str()).collect();
            if !names.is_empty() {
                let mut args = vec!["-q", "--qf", "%{NAME} %{VERSION}-%{RELEASE}\\n", "--"];
                args.extend(&names);
                let now: BTreeMap<String, String> = run_quiet("rpm", &args, 30)
                    .map(|d| d.stdout.lines().filter_map(|l| l.split_once(' ')).map(|(a, b)| (a.into(), b.into())).collect())
                    .unwrap_or_default();
                for u in &mut list {
                    u.from = now.get(&u.name).cloned();
                }
            }
            list
        }
        Manager::Pacman if exec::installed("checkupdates") => {
            // Exits 2 when there is nothing to update.
            let done = run_quiet("checkupdates", &[], 300)?;
            ensure!(done.success() || done.status.code() == Some(2), "checkupdates: {}", done.complaint());
            parse_pacman_updates(&done.stdout)
        }
        Manager::Pacman => {
            let done = run_quiet("pacman", &["-Qu"], 60)?;
            parse_pacman_updates(&done.stdout)
        }
    })
}

/// A process still running code an update replaced.
#[derive(Debug, PartialEq)]
struct Stale {
    pid: u32,
    name: String,
    /// The systemd unit it belongs to, and whether that's a user unit.
    unit: Option<(String, bool)>,
    files: Vec<String>,
}

/// Replaced system files in a `/proc/<pid>/maps` (and the exe link).
fn deleted_files(maps: &str, exe: Option<&str>) -> Vec<String> {
    const SYSTEM: [&str; 7] = ["/usr/", "/lib/", "/lib64/", "/lib32/", "/bin/", "/sbin/", "/opt/"];
    let mut files: BTreeSet<String> = BTreeSet::new();
    // "addr perms offset dev inode      path": the path is padded and may hold spaces.
    let paths = maps.lines().filter_map(|l| l.splitn(6, ' ').nth(5).map(str::trim));
    for p in paths.chain(exe) {
        if let Some(path) = p.strip_suffix(" (deleted)") {
            if SYSTEM.iter().any(|s| path.starts_with(s)) {
                files.insert(path.to_string());
            }
        }
    }
    files.into_iter().collect()
}

/// `0::/system.slice/ssh.service` → ssh.service (system);
/// `…/user@1000.service/app.slice/pipewire.service` → pipewire.service (user).
fn unit_of(cgroup: &str) -> Option<(String, bool)> {
    let path = cgroup.lines().find_map(|l| l.strip_prefix("0::"))?;
    let unit = path.rsplit('/').find(|c| c.ends_with(".service") || c.ends_with(".scope"))?;
    if unit.ends_with(".scope") || unit.starts_with("user@") {
        return None;
    }
    Some((unit.to_string(), path.contains("/user@")))
}

fn stale_processes(proc: &Path) -> (Vec<Stale>, usize) {
    let mut out = Vec::new();
    let mut unreadable = 0;
    let Ok(dir) = std::fs::read_dir(proc) else { return (out, 1) };
    for e in dir.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let base = e.path();
        let maps = match std::fs::read_to_string(base.join("maps")) {
            Ok(m) => m,
            // A process that exited mid-scan is no loss.
            Err(e) => {
                unreadable += usize::from(e.kind() == std::io::ErrorKind::PermissionDenied);
                continue;
            }
        };
        let exe = std::fs::read_link(base.join("exe")).ok().map(|p| p.to_string_lossy().into_owned());
        let files = deleted_files(&maps, exe.as_deref());
        if files.is_empty() {
            continue;
        }
        let name = std::fs::read_to_string(base.join("comm")).unwrap_or_default().trim().to_string();
        let unit = std::fs::read_to_string(base.join("cgroup")).ok().as_deref().and_then(unit_of);
        out.push(Stale { pid, name, unit, files });
    }
    out.sort_by_key(|s| s.pid);
    (out, unreadable)
}

/// Whether the machine needs a reboot, and why.
fn reboot_reasons(stale: &[Stale]) -> Vec<String> {
    let mut why = Vec::new();
    if Path::new("/run/reboot-required").exists() || Path::new("/var/run/reboot-required").exists() {
        let pkgs = std::fs::read_to_string("/run/reboot-required.pkgs")
            .or_else(|_| std::fs::read_to_string("/var/run/reboot-required.pkgs"))
            .unwrap_or_default();
        let pkgs: BTreeSet<&str> = pkgs.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        why.push(if pkgs.is_empty() {
            "the package manager asked for one".to_string()
        } else {
            format!("updated: {}", pkgs.into_iter().collect::<Vec<_>>().join(", "))
        });
    }
    // The running kernel's modules are gone: a new kernel replaced it.
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let release = release.trim();
    for modules in ["/usr/lib/modules", "/lib/modules"] {
        let others = std::fs::read_dir(modules).map(|d| d.flatten().count()).unwrap_or(0);
        if others > 0 && !release.is_empty() && !Path::new(modules).join(release).exists() {
            why.push(format!("the running kernel ({release}) has been replaced"));
            break;
        }
    }
    if let Some(init) = stale.iter().find(|s| s.pid == 1) {
        why.push(format!("{} (PID 1) runs replaced libraries", init.name));
    }
    if why.is_empty() && exec::installed("needs-restarting") {
        // dnf-utils: exits 1 when core packages changed since boot.
        if run_quiet("needs-restarting", &["-r"], 60).is_ok_and(|d| d.status.code() == Some(1)) {
            why.push("needs-restarting reports core packages updated since boot".into());
        }
    }
    why.dedup();
    why
}

/// What still runs old code: services to restart, other processes, reboot.
fn after_effects() -> Value {
    let (stale, unreadable) = stale_processes(Path::new("/proc"));
    let reboot = reboot_reasons(&stale);
    let mut system: BTreeSet<&str> = BTreeSet::new();
    let mut user: BTreeSet<&str> = BTreeSet::new();
    for s in stale.iter().filter(|s| s.pid != 1) {
        if let Some((unit, is_user)) = &s.unit {
            if *is_user { &mut user } else { &mut system }.insert(unit);
        }
    }
    let others: Vec<Value> = stale
        .iter()
        .filter(|s| s.unit.is_none() && s.pid != 1)
        .take(30)
        .map(|s| json!({ "pid": s.pid, "name": s.name, "files": s.files.iter().take(3).collect::<Vec<_>>() }))
        .collect();
    let mut out = json!({
        "reboot_required": !reboot.is_empty(),
        "reboot_reasons": reboot,
        "restart_services": system,
        "restart_user_services": user,
        "restart_processes": others,
    });
    if unreadable > 0 {
        out["note"] = json!(format!(
            "{unreadable} process(es) could not be inspected (Orion is not root, or lacks ptrace access), \
             so the restart lists may be incomplete"
        ));
    }
    out
}

pub struct UpdateCheck;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckArgs {
    #[serde(default = "yes")]
    refresh: bool,
}

fn yes() -> bool {
    true
}

impl Tool for UpdateCheck {
    fn name(&self) -> &'static str {
        "update_check"
    }

    fn description(&self) -> &'static str {
        "Check for system updates: refreshes the package lists, then lists every package that can be \
         upgraded (from and to which version), marking security fixes and kernel updates. Also reports \
         whether the machine needs a reboot and which services still run replaced code and need a restart."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "refresh": { "type": "boolean", "description": "Fetch the latest package lists first (default true)." }
            },
            "additionalProperties": false
        })
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: CheckArgs = parse_args(args)?;
        let m = manager(policy)?;
        let refresh_error = if a.refresh { refresh(policy, m) } else { None };
        let mut list = pending(m)?;
        list.sort_by(|a, b| (!a.security, &a.name).cmp(&(!b.security, &b.name)));
        let security = list.iter().filter(|u| u.security).count();
        let mut out = json!({
            "manager": m.name(),
            "refreshed": a.refresh && refresh_error.is_none(),
            "count": list.len(),
            "security": security,
            "kernel": list.iter().any(|u| kernel_package(&u.name)),
            "updates": list.iter().map(Update::json).collect::<Vec<_>>(),
        });
        if m == Manager::Pacman {
            out["note"] = json!("Arch publishes no security metadata, so no update is marked as a security fix");
        }
        if let Some(e) = refresh_error {
            out["refresh_error"] = json!(e);
        }
        if let Value::Object(after) = after_effects() {
            out.as_object_mut().unwrap().extend(after);
        }
        Ok(out)
    }
}

pub struct UpdateApply;

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ApplyArgs {
    #[serde(default)]
    packages: Vec<String>,
    #[serde(default)]
    security_only: bool,
    #[serde(default)]
    full: bool,
}

/// What an update run will do, worked out before any approval.
struct Prepared {
    m: Manager,
    /// Only these packages (empty: everything).
    only: Vec<String>,
    plan: Option<UpgradePlan>,
    full: bool,
}

fn guard_removals(policy: &Policy, m: Manager, plan: &UpgradePlan) -> Result<()> {
    for (name, _) in &plan.remove {
        let bare = name.split(':').next().unwrap_or(name);
        if let Some(p) = policy.packages.protected.iter().find(|p| glob_match(p, bare)) {
            bail!("this update would remove {name}, which is protected (packages.protected: {p:?})");
        }
        if m == Manager::Apt && essential(name) {
            bail!("this update would remove {name}, which the system needs");
        }
    }
    Ok(())
}

fn apt_argv(only: &[String], full: bool, simulate: bool) -> Vec<String> {
    let mut v: Vec<String> = if simulate { vec!["apt-get".into(), "-s".into()] } else { vec![] };
    if !simulate {
        v = ["DEBIAN_FRONTEND=noninteractive", "apt-get", "-y", "-q", "-o", "Dpkg::Options::=--force-confold"]
            .map(String::from)
            .to_vec();
    }
    if only.is_empty() {
        v.push(if full { "dist-upgrade" } else { "upgrade" }.into());
        if !full {
            // New dependencies (a new kernel's packages) are fine; removals are not.
            v.push("--with-new-pkgs".into());
        }
    } else {
        v.extend(["install".into(), "--only-upgrade".into(), "--".into()]);
        v.extend(only.iter().cloned());
    }
    v
}

fn prepare(policy: &Policy, args: &Value) -> Result<Prepared> {
    let a: ApplyArgs = parse_args(args.clone())?;
    ensure!(a.packages.len() <= 200, "name at most 200 packages");
    for p in &a.packages {
        package_name(p)?;
    }
    ensure!(
        a.packages.is_empty() || !a.security_only,
        "give either packages or security_only, not both"
    );
    let m = manager(policy)?;
    if m == Manager::Pacman {
        ensure!(a.packages.is_empty(), "Arch does not support upgrading only some packages; upgrade everything");
        ensure!(!a.security_only, "Arch publishes no security metadata; upgrade everything");
    }
    let mut only = a.packages;
    if a.security_only && m == Manager::Apt {
        only = pending(m)?.into_iter().filter(|u| u.security).map(|u| u.name).collect();
    }
    let plan = if m == Manager::Apt && !(a.security_only && only.is_empty()) {
        let argv = apt_argv(&only, a.full, true);
        let refs: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
        let text = exec::output("apt-get", &refs).map_err(|e| {
            let msg = format!("{e:#}");
            let reason: Vec<&str> = msg.lines().filter(|l| l.contains("E: ")).collect();
            anyhow::anyhow!("{}", if reason.is_empty() { msg.clone() } else { reason.join("; ") })
        })?;
        let plan = parse_apt_upgrade(&text);
        guard_removals(policy, m, &plan)?;
        Some(plan)
    } else {
        None
    };
    Ok(Prepared { m, only, plan, full: a.full })
}

impl Tool for UpdateApply {
    fn name(&self) -> &'static str {
        "update_apply"
    }

    fn description(&self) -> &'static str {
        "Install system updates, as root and outside the sandbox: everything, only security fixes \
         (security_only), or only the named packages. Never removes packages unless full is set (apt \
         dist-upgrade), and even then never one the system needs or the policy protects. Returns what \
         was upgraded and whether services need a restart or the machine a reboot. Use update_check first."
    }

    fn risk(&self) -> Risk {
        Risk::Execute
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "packages": { "type": "array", "items": { "type": "string" }, "maxItems": 200,
                              "description": "Upgrade only these installed packages. Default: everything." },
                "security_only": { "type": "boolean", "description": "Only updates that fix security problems." },
                "full": { "type": "boolean",
                          "description": "Also allow upgrades that remove or replace packages (apt dist-upgrade). \
                                          Default false: such upgrades are kept back." }
            },
            "additionalProperties": false
        })
    }

    fn preapproval(&self, policy: &Policy, args: &Value) -> Preapproval {
        let a: ApplyArgs = match parse_args(args.clone()) {
            Ok(a) => a,
            Err(e) => return Preapproval::Refused(format!("{e:#}")),
        };
        let mode = policy.packages.updates.as_str();
        match mode {
            _ if a.full => Preapproval::Refused("updates that may remove packages always ask".into()),
            "all" => Preapproval::Granted,
            "security" if a.security_only => Preapproval::Granted,
            "security" => Preapproval::Refused("packages.updates allows only security_only updates".into()),
            "ask" => Preapproval::NotApplicable,
            other => Preapproval::Refused(format!("packages.updates must be ask, security or all, not {other:?}")),
        }
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        prepare(policy, args).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let p = prepare(policy, &args)?;
        let a: ApplyArgs = parse_args(args)?;
        if p.plan.as_ref().is_some_and(|plan| plan.upgrade.is_empty() && plan.install.is_empty() && plan.remove.is_empty())
            || (a.security_only && p.m == Manager::Apt && p.only.is_empty())
        {
            let mut out = json!({ "ok": true, "manager": p.m.name(), "upgraded": [], "message": "nothing to update" });
            if let Some(plan) = &p.plan {
                out["kept_back"] = json!(plan.kept_back);
            }
            return Ok(out);
        }
        let argv: Vec<String> = match p.m {
            Manager::Apt => apt_argv(&p.only, p.full, false),
            Manager::Dnf => {
                let mut v: Vec<String> = ["dnf", "upgrade", "-y", "-q"].map(String::from).to_vec();
                if a.security_only {
                    v.push("--security".into());
                }
                if !p.only.is_empty() {
                    v.push("--".into());
                    v.extend(p.only.iter().cloned());
                }
                v
            }
            Manager::Pacman => ["pacman", "-Syu", "--noconfirm"].map(String::from).to_vec(),
        };
        let (done, elevated) = run_as_root(policy, p.m, argv)?;
        let mut out = json!({
            "ok": done.success(),
            "manager": p.m.name(),
            "elevated": elevated,
            "duration_secs": done.duration.as_secs(),
            "output": tail(&done.stdout),
        });
        if let Some(plan) = &p.plan {
            if done.success() {
                out["upgraded"] = json!(plan.upgrade.iter().map(Update::json).collect::<Vec<_>>());
                out["installed"] = json!(plan.install.iter().map(|(n, v)| json!({ "name": n, "version": v })).collect::<Vec<_>>());
                out["removed"] = json!(plan.remove.iter().map(|(n, v)| json!({ "name": n, "version": v })).collect::<Vec<_>>());
            }
            out["kept_back"] = json!(plan.kept_back);
        }
        if !done.success() {
            out["error"] = json!(complaint(&done));
        }
        if let Value::Object(after) = after_effects() {
            out.as_object_mut().unwrap().extend(after);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_upgradable_lists() {
        let apt = parse_apt_upgradable(
            "Listing...\n\
             curl/noble-updates,noble-security 8.5.0-2ubuntu10.16 amd64 [upgradable from: 8.5.0-2ubuntu10.15]\n\
             docker-ce/noble 5:29.9.1-1~ubuntu.24.04~noble amd64 [upgradable from: 5:29.8.2-1~ubuntu.24.04~noble]\n",
        );
        assert_eq!(apt.len(), 2);
        assert_eq!(apt[0].name, "curl");
        assert_eq!(apt[0].from.as_deref(), Some("8.5.0-2ubuntu10.15"));
        assert_eq!(apt[0].to, "8.5.0-2ubuntu10.16");
        assert!(apt[0].security);
        assert!(!apt[1].security);

        let security = parse_dnf_security("FEDORA-2024-1a2b3c Important/Sec. curl-8.6.0-1.fc40.x86_64\n");
        assert_eq!(security, BTreeSet::from(["curl".to_string()]));
        let dnf = parse_dnf_updates(
            "\ncurl.x86_64        8.6.0-1.fc40       updates\nkernel-core.x86_64 6.9.4-200.fc40 updates\n\
             Obsoleting Packages\ngrub2-tools.x86_64 1:2.06-1 updates\n",
            &security,
        );
        assert_eq!(dnf.len(), 2);
        assert!(dnf[0].security && !dnf[1].security);
        assert!(kernel_package(&dnf[1].name));

        let pac = parse_pacman_updates("curl 8.5.0-1 -> 8.6.0-1\nlinux 6.9.1.arch1-1 -> 6.9.2.arch1-1\n");
        assert_eq!(pac[1], Update { name: "linux".into(), from: Some("6.9.1.arch1-1".into()), to: "6.9.2.arch1-1".into(), ..Default::default() });
    }

    #[test]
    fn parses_upgrade_simulation() {
        let plan = parse_apt_upgrade(
            "Reading package lists...\n\
             The following packages have been kept back:\n  docker-ce docker-ce-cli\n\
             The following packages will be upgraded:\n  curl fonts-opensymbol\n\
             Inst curl [8.5.0-2ubuntu10.15] (8.5.0-2ubuntu10.16 Ubuntu:24.04/noble-security [amd64])\n\
             Inst fonts-opensymbol [4:102.12-6] (4:102.12-7 Ubuntu:24.04/noble-updates, Ubuntu:24.04/noble-security [all])\n\
             Inst linux-image-6.8.0-50-generic (6.8.0-50.51 Ubuntu:24.04/noble-updates [amd64])\n\
             Remv oldlib [1.0]\n\
             Conf curl (8.5.0-2ubuntu10.16 Ubuntu:24.04/noble-security [amd64])\n",
        );
        assert_eq!(plan.kept_back, ["docker-ce", "docker-ce-cli"]);
        assert_eq!(plan.upgrade.len(), 2);
        assert_eq!(plan.upgrade[0].from.as_deref(), Some("8.5.0-2ubuntu10.15"));
        assert_eq!(plan.upgrade[0].origin, "noble-security");
        assert_eq!(plan.upgrade[1].origin, "noble-updates,noble-security");
        assert!(plan.upgrade[1].security);
        assert_eq!(plan.install, [("linux-image-6.8.0-50-generic".into(), "6.8.0-50.51".into())]);
        assert_eq!(plan.remove, [("oldlib".into(), "1.0".into())]);
    }

    #[test]
    fn finds_replaced_code() {
        let maps = "7f00-7f01 r-xp 00000000 08:01 123  /usr/lib/x86_64-linux-gnu/libssl.so.3 (deleted)\n\
                    7f01-7f02 r--p 00000000 08:01 124  /usr/lib/x86_64-linux-gnu/libc.so.6\n\
                    7f02-7f03 rw-s 00000000 00:01 9    /memfd:pulseaudio (deleted)\n\
                    7f03-7f04 r-xp 00000000 08:01 125  /home/me/target/debug/app (deleted)\n\
                    7f04-7f05 rw-p 00000000 00:00 0\n";
        assert_eq!(deleted_files(maps, Some("/usr/sbin/sshd (deleted)")), ["/usr/lib/x86_64-linux-gnu/libssl.so.3", "/usr/sbin/sshd"]);
        assert!(deleted_files(maps.lines().nth(1).unwrap(), Some("/usr/bin/bash")).is_empty());

        assert_eq!(unit_of("0::/system.slice/ssh.service\n"), Some(("ssh.service".into(), false)));
        assert_eq!(
            unit_of("0::/user.slice/user-1000.slice/user@1000.service/app.slice/pipewire.service\n"),
            Some(("pipewire.service".into(), true))
        );
        assert_eq!(unit_of("0::/user.slice/user-1000.slice/session-2.scope\n"), None);
        assert_eq!(unit_of("0::/\n"), None);
    }

    #[test]
    fn update_preapproval() {
        let p = |mode: &str| Policy::from_toml(&format!("[packages]\nupdates = \"{mode}\"\n")).unwrap();
        let granted = |p: &Policy, a: Value| matches!(UpdateApply.preapproval(p, &a), Preapproval::Granted);
        assert!(!granted(&p("ask"), json!({ "security_only": true })));
        assert!(granted(&p("security"), json!({ "security_only": true })));
        assert!(!granted(&p("security"), json!({})));
        assert!(granted(&p("all"), json!({ "packages": ["curl"] })));
        assert!(!granted(&p("all"), json!({ "full": true })));
        assert!(!granted(&p("sometimes"), json!({})));
    }

    #[test]
    fn checks_against_the_real_manager() {
        if !exec::installed("apt-get") || !exec::installed("dpkg-query") {
            return;
        }
        let p = Policy::from_toml("").unwrap();
        let r = UpdateCheck.call(&p, json!({ "refresh": false })).unwrap();
        assert_eq!(r["manager"], "apt");
        assert_eq!(r["count"].as_u64().unwrap() as usize, r["updates"].as_array().unwrap().len(), "{r}");
        assert!(r["reboot_required"].is_boolean(), "{r}");

        // The plan is worked out before approval; bad names never reach apt.
        assert!(UpdateApply.precheck(&p, &json!({ "packages": ["-y"] })).is_err());
        assert!(UpdateApply.precheck(&p, &json!({ "packages": ["curl"], "security_only": true })).is_err());
        assert!(UpdateApply.precheck(&p, &json!({ "security_only": true })).is_ok());
    }
}
