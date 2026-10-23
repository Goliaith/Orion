//! System updates: what can be upgraded (and which of it fixes security
//! problems), upgrading, and what still runs old code afterwards: the
//! services to restart and whether the machine needs a reboot.
//!
//! Like `pkg_install`, upgrades run as root and outside the sandbox. Every
//! change is worked out first (`apt-get -s`, `dnf --assumeno`, `zypper
//! --dry-run`, `pacman -p`), and refused before any approval if it would
//! remove a package the system needs or the policy protects. `pkg_install`
//! and `pkg_remove` use the same dry run.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::exec::{self, glob_match};
use super::packages::{complaint, essential, manager, package_name, parse_fields, run_as_root, tail, xml_elements, xml_unescape, Manager};
use crate::policy::Policy;
use crate::tool::{parse_args, Preapproval, Risk, Tool};

/// One package that can be upgraded.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Update {
    pub name: String,
    pub from: Option<String>,
    pub to: String,
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
    ["linux", "linux-lts", "linux-zen", "linux-hardened", "kernel", "kernel-core", "kernel-default"].contains(&name)
        || ["linux-image-", "linux-modules-", "kernel-core-", "kernel-modules", "kernel-default-"]
            .iter()
            .any(|p| name.starts_with(p))
}

fn security_origin(origin: &str) -> bool {
    origin.to_ascii_lowercase().contains("security")
}

const ARCHES: [&str; 12] =
    ["noarch", "x86_64", "aarch64", "i686", "i586", "i386", "ppc64le", "ppc64", "s390x", "armv7hl", "riscv64", "src"];

/// `curl.x86_64` → `curl`.
fn strip_arch(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((n, arch)) if ARCHES.contains(&arch) => n,
        _ => name,
    }
}

/// What a change would do, from the package manager's own dry run.
#[derive(Debug, Default)]
pub(super) struct Plan {
    pub upgrade: Vec<Update>,
    /// New packages (named ones, dependencies, a new kernel, a replacement).
    pub install: Vec<(String, String)>,
    pub remove: Vec<(String, String)>,
    /// Upgradable, but left alone: held, phased, or needing a removal.
    kept_back: Vec<String>,
}

impl Plan {
    fn is_empty(&self) -> bool {
        self.upgrade.is_empty() && self.install.is_empty() && self.remove.is_empty()
    }
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

/// `apt-get -s install|remove|upgrade|dist-upgrade`.
fn parse_apt_plan(text: &str) -> Plan {
    let mut plan = Plan::default();
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
/// header (dnf5: "Upgrades") and before any "Obsoleting Packages" section.
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

/// `dnf updateinfo list --security --updates`, the package names:
/// dnf4 `FEDORA-2024-1a2b3c  Important/Sec.  curl-8.6.0-1.fc40.x86_64`,
/// dnf5 `FEDORA-2024-1a2b3c  security  Important  curl-8.6.0-1.fc40.x86_64  2024-06-01 00:00:00`.
fn parse_dnf_security(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|l| {
            let nevra = l.split_whitespace().skip(1).find(|f| strip_arch(f) != *f)?;
            let (rest, _release) = strip_arch(nevra).rsplit_once('-')?;
            Some(rest.rsplit_once('-')?.0.to_string())
        })
        .collect()
}

/// The transaction table `dnf --assumeno` prints. dnf4:
/// ```text
/// Upgrading:
///  curl        x86_64   8.6.0-1.fc40   updates   300 k
/// Installing:
///  orion-new   noarch   2.0-1          test        6 k
///      replacing  orion-old.noarch 1.0-1
/// Removing:
///  foo         x86_64   1.0-1          @System    10 k
/// ```
/// dnf5 is alike, with epochs (`0:2.0-1`) and `replacing orion-old noarch 0:1.0-1 …`.
/// A package replacing an older version of itself is an upgrade; one
/// replacing another package removes that package.
fn parse_dnf_transaction(text: &str) -> Plan {
    #[derive(PartialEq)]
    enum Section {
        Other,
        Upgrade,
        Install,
        Remove,
    }
    let version = |v: &str| v.strip_prefix("0:").unwrap_or(v).to_string();
    let mut plan = Plan::default();
    let mut section = Section::Other;
    let mut last: Option<String> = None;
    for line in text.lines() {
        if line.starts_with("Transaction Summary") {
            break;
        }
        if !line.starts_with(' ') && line.ends_with(':') {
            section = match line {
                l if l.starts_with("Upgrading") || l.starts_with("Downgrading") => Section::Upgrade,
                l if l.starts_with("Installing") => Section::Install,
                l if l.starts_with("Removing") => Section::Remove,
                _ => Section::Other,
            };
            last = None;
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.first() == Some(&"replacing") && f.len() >= 3 {
            let (name, ver) = if f.len() >= 4 && ARCHES.contains(&f[2]) { (f[1], f[3]) } else { (strip_arch(f[1]), f[2]) };
            if last.as_deref() == Some(name) {
                if let Some(u) = plan.upgrade.iter_mut().find(|u| u.name == name) {
                    u.from = Some(version(ver));
                }
            } else {
                plan.remove.push((name.to_string(), version(ver)));
            }
            continue;
        }
        if !line.starts_with(' ') || f.len() < 4 || section == Section::Other {
            continue;
        }
        let (name, ver, repo) = (f[0].to_string(), version(f[2]), f[3].to_string());
        last = Some(name.clone());
        match section {
            Section::Upgrade => plan.upgrade.push(Update { name, to: ver, origin: repo, ..Default::default() }),
            Section::Install => plan.install.push((name, ver)),
            Section::Remove => plan.remove.push((name, ver)),
            Section::Other => {}
        }
    }
    plan
}

/// `zypper --xmlout list-updates`.
fn parse_zypper_updates(xml: &str, security: &BTreeSet<String>) -> Vec<Update> {
    xml.split("<update ")
        .skip(1)
        .filter_map(|chunk| {
            let a = xml_elements(&format!("<update {chunk}"), "update").next()?;
            if a.get("kind").is_some_and(|k| k != "package") {
                return None;
            }
            let name = a.get("name")?.clone();
            let origin = xml_elements(chunk, "source").next().and_then(|s| s.get("alias").cloned()).unwrap_or_default();
            Some(Update {
                security: security.contains(&name),
                from: a.get("edition-old").cloned(),
                to: a.get("edition").cloned().unwrap_or_default(),
                origin,
                name,
            })
        })
        .collect()
}

/// The `<install-summary>` of a `zypper --xmlout … --dry-run`.
fn parse_zypper_plan(xml: &str) -> Plan {
    let mut plan = Plan::default();
    let Some(start) = xml.find("<install-summary") else { return plan };
    let summary = &xml[start..xml[start..].find("</install-summary>").map_or(xml.len(), |e| start + e)];
    for chunk in summary.split("<to-").skip(1) {
        let section = chunk.split(['>', ' ']).next().unwrap_or_default();
        for a in xml_elements(chunk, "solvable").filter(|a| a.get("type").is_none_or(|t| t == "package")) {
            let name = a.get("name").cloned().unwrap_or_default();
            let edition = a.get("edition").cloned().unwrap_or_default();
            match section {
                "upgrade" | "downgrade" | "change-arch" | "change-vendor" => plan.upgrade.push(Update {
                    name,
                    from: a.get("edition-old").cloned(),
                    to: edition,
                    origin: a.get("repository").cloned().unwrap_or_default(),
                    security: false,
                }),
                "install" | "reinstall" => plan.install.push((name, edition)),
                "remove" => plan.remove.push((name, edition)),
                _ => {}
            }
        }
    }
    plan
}

/// The packages the security patches fix, from `zypper info -t patch`:
/// `Conflicts : orion-sec < 1.1-1` and continuation lines like it.
fn parse_zypper_patch_packages(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut in_conflicts = false;
    for line in text.lines() {
        let item = match line.split_once(" : ") {
            Some((key, value)) if !key.starts_with(' ') => {
                in_conflicts = key.trim() == "Conflicts";
                value
            }
            _ if in_conflicts && line.starts_with(' ') => line,
            _ => {
                in_conflicts = false;
                continue;
            }
        };
        if in_conflicts {
            if let Some(name) = item.split_whitespace().next().filter(|n| !n.is_empty()) {
                out.insert(strip_arch(name.trim_start_matches("srcpackage:")).to_string());
            }
        }
    }
    out
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

/// `pacman -Si`: each package's `Replaces` list (`None` when empty).
fn parse_pacman_replaces(text: &str) -> BTreeMap<String, Vec<String>> {
    text.split("\n\n")
        .filter_map(|block| {
            let f = parse_fields(block);
            let replaces = f.get("Replaces")?.split_whitespace().filter(|r| *r != "None");
            // "foo>=1.0" → foo
            let replaces = replaces.map(|r| r.split(['<', '>', '=']).next().unwrap_or(r).to_string()).collect();
            Some((f.get("Name")?.clone(), replaces))
        })
        .collect()
}

fn run_quiet(program: &str, args: &[&str], secs: u64) -> Result<exec::Finished> {
    let mut cmd = Command::new(program);
    cmd.args(args).env("LC_ALL", "C");
    exec::run(cmd, None, Duration::from_secs(secs), 4 << 20)
}

fn tumbleweed() -> bool {
    let os = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    os.lines().any(|l| l.starts_with("ID=") && ["tumbleweed", "slowroll", "microos"].iter().any(|t| l.contains(t)))
}

/// A private copy of pacman's sync databases, so checking never sets up a
/// partial upgrade (`pacman -Sy` alone would). The same idea as checkupdates.
fn pacman_db() -> Option<PathBuf> {
    Policy::state_dir().map(|d| d.join("pacman-db"))
}

fn sync_pacman_db() -> Result<()> {
    let dir = pacman_db().context("no state directory for a private pacman database")?;
    std::fs::create_dir_all(&dir)?;
    let local = dir.join("local");
    if std::fs::symlink_metadata(&local).is_err() {
        std::os::unix::fs::symlink("/var/lib/pacman/local", &local)?;
    }
    let dir = dir.to_string_lossy();
    let mut args = vec!["pacman", "-Sy", "--dbpath", &dir, "--logfile", "/dev/null"];
    // SAFETY: geteuid has no preconditions.
    let root = unsafe { libc::geteuid() } == 0;
    if !root {
        ensure!(exec::installed("fakeroot"), "refreshing a private pacman database needs root or fakeroot");
        args.insert(0, "fakeroot");
        args.insert(1, "--");
    }
    let mut done = run_quiet(args[0], &args[1..], 300)?;
    if root && !done.success() && done.stderr.contains("Permission denied") {
        // pacman 7 downloads as an unprivileged user (DownloadUser), which
        // can't write into root's state directory. The lists are signed.
        args.push("--disable-sandbox");
        done = run_quiet(args[0], &args[1..], 300)?;
    }
    ensure!(done.success(), "pacman -Sy: {}", done.complaint());
    Ok(())
}

/// `--dbpath <private copy>`, once a refresh has filled one.
fn pacman_db_args() -> Vec<String> {
    let synced = |d: &PathBuf| {
        std::fs::read_dir(d.join("sync")).is_ok_and(|e| e.flatten().any(|f| f.file_name().to_string_lossy().ends_with(".db")))
    };
    match pacman_db().filter(synced) {
        Some(d) => vec!["--dbpath".into(), d.to_string_lossy().into_owned()],
        None => Vec::new(),
    }
}

/// Fetch the latest package lists. Returns why it couldn't, if it couldn't.
fn refresh(policy: &Policy, m: Manager) -> Option<String> {
    let argv: Vec<String> = match m {
        Manager::Apt => ["DEBIAN_FRONTEND=noninteractive", "apt-get", "update", "-q"].map(String::from).to_vec(),
        Manager::Dnf => ["dnf", "makecache", "-q"].map(String::from).to_vec(),
        Manager::Zypper => ["zypper", "-n", "-q", "refresh"].map(String::from).to_vec(),
        Manager::Pacman => return sync_pacman_db().err().map(|e| format!("{e:#}")),
    };
    match run_as_root(policy, m, argv) {
        Ok((done, _)) if done.success() => None,
        Ok((done, _)) => Some(complaint(&done)),
        Err(e) => Some(format!("{e:#}")),
    }
}

/// The names of packages with a pending security fix (dnf and zypper).
fn security_names(m: Manager) -> BTreeSet<String> {
    match m {
        Manager::Dnf => run_quiet("dnf", &["-q", "updateinfo", "list", "--security", "--updates"], 300)
            .map(|d| parse_dnf_security(&d.stdout))
            .unwrap_or_default(),
        Manager::Zypper => {
            let patches: Vec<String> = run_quiet("zypper", &["-n", "--xmlout", "list-patches", "--category", "security"], 300)
                .map(|d| {
                    xml_elements(&d.stdout, "update")
                        .filter(|a| a.get("status").is_none_or(|s| s == "needed"))
                        .filter_map(|mut a| a.remove("name"))
                        .collect()
                })
                .unwrap_or_default();
            if patches.is_empty() {
                return BTreeSet::new();
            }
            let mut args = vec!["-n", "-q", "info", "-t", "patch", "--"];
            args.extend(patches.iter().map(String::as_str));
            run_quiet("zypper", &args, 300).map(|d| parse_zypper_patch_packages(&d.stdout)).unwrap_or_default()
        }
        Manager::Apt | Manager::Pacman => BTreeSet::new(),
    }
}

fn pending(m: Manager) -> Result<Vec<Update>> {
    Ok(match m {
        Manager::Apt => parse_apt_upgradable(&exec::output("apt", &["list", "--upgradable"])?),
        Manager::Dnf => {
            // check-update exits 100 when there are updates.
            let done = run_quiet("dnf", &["-q", "check-update"], 300)?;
            ensure!(done.success() || done.status.code() == Some(100), "dnf check-update: {}", done.complaint());
            let mut list = parse_dnf_updates(&done.stdout, &security_names(m));
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
        Manager::Zypper => {
            let done = run_quiet("zypper", &["-n", "--xmlout", "list-updates"], 300)?;
            ensure!(done.success(), "zypper list-updates: {}", done.complaint());
            parse_zypper_updates(&done.stdout, &security_names(m))
        }
        Manager::Pacman => {
            // Exits 1 when there is nothing to update.
            let mut args = pacman_db_args();
            args.push("-Qu".into());
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            parse_pacman_updates(&run_quiet("pacman", &refs, 60)?.stdout)
        }
    })
}

/// A change to the installed packages.
#[derive(Clone, Copy, Debug)]
pub(super) enum Action {
    Install,
    Remove,
    Upgrade { security_only: bool, full: bool },
    /// Install exact versions again, older ones included (a rollback):
    /// `name=version` (apt, zypper), `name-version` (dnf), or pacman's
    /// cached package files.
    Restore,
}

/// The command line for `action` on `packages`: the real one, or (`dry`)
/// the dry run that shows what it would do.
pub(super) fn command(m: Manager, action: Action, packages: &[String], dry: bool) -> Vec<String> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    let mut v = match (m, dry) {
        (Manager::Apt, true) => s(&["apt-get", "-s"]),
        (Manager::Apt, false) => {
            s(&["DEBIAN_FRONTEND=noninteractive", "apt-get", "-y", "-q", "-o", "Dpkg::Options::=--force-confold"])
        }
        (Manager::Dnf, true) => s(&["dnf", "--assumeno"]),
        (Manager::Dnf, false) => s(&["dnf", "-y", "-q"]),
        (Manager::Zypper, true) => s(&["zypper", "-n", "--xmlout"]),
        (Manager::Zypper, false) => s(&["zypper", "-n"]),
        (Manager::Pacman, _) => s(&["pacman"]),
    };
    let rest = match (m, action) {
        (Manager::Apt, Action::Install) => s(&["install"]),
        (Manager::Apt, Action::Restore) => s(&["--allow-downgrades", "install"]),
        (Manager::Apt, Action::Remove) => s(&["remove"]),
        (Manager::Apt, Action::Upgrade { .. }) if !packages.is_empty() => s(&["install", "--only-upgrade"]),
        // New dependencies (a new kernel's packages) are fine; removals are not.
        (Manager::Apt, Action::Upgrade { full: false, .. }) => s(&["upgrade", "--with-new-pkgs"]),
        (Manager::Apt, Action::Upgrade { full: true, .. }) => s(&["dist-upgrade"]),
        // dnf installs an older version named exactly by downgrading to it.
        (Manager::Dnf, Action::Install | Action::Restore) => s(&["install"]),
        (Manager::Dnf, Action::Remove) => s(&["remove"]),
        (Manager::Dnf, Action::Upgrade { security_only, .. }) => {
            if security_only { s(&["upgrade", "--security"]) } else { s(&["upgrade"]) }
        }
        (Manager::Zypper, Action::Install) => s(&["install"]),
        (Manager::Zypper, Action::Restore) => s(&["install", "--oldpackage"]),
        (Manager::Zypper, Action::Remove) => s(&["remove"]),
        (Manager::Zypper, Action::Upgrade { security_only: true, .. }) => s(&["patch", "--category", "security"]),
        // Tumbleweed is only ever upgraded as a whole distribution.
        (Manager::Zypper, Action::Upgrade { .. }) if packages.is_empty() && tumbleweed() => s(&["dist-upgrade"]),
        (Manager::Zypper, Action::Upgrade { .. }) => s(&["update"]),
        (Manager::Pacman, Action::Install) => s(&[if dry { "-Sp" } else { "-S" }, "--needed"]),
        (Manager::Pacman, Action::Remove) => s(&[if dry { "-Rp" } else { "-R" }]),
        (Manager::Pacman, Action::Restore) => s(&[if dry { "-Up" } else { "-U" }]),
        (Manager::Pacman, Action::Upgrade { .. }) => s(&[if dry { "-Sup" } else { "-Syu" }]),
    };
    v.extend(rest);
    match m {
        Manager::Zypper if dry => v.push("--dry-run".into()),
        Manager::Pacman if dry => {
            v.extend(pacman_db_args().into_iter().filter(|_| matches!(action, Action::Upgrade { .. })));
            v.extend(["--print-format".into(), "%n %v".into()]);
        }
        Manager::Pacman => v.push("--noconfirm".into()),
        _ => {}
    }
    if !packages.is_empty() {
        v.push("--".into());
        v.extend(packages.iter().cloned());
    }
    v
}

/// The error lines of a failed dry run (`E: …`, `Error: …`, zypper's XML messages).
fn dry_run_error(program: &str, text: &str) -> String {
    let xml: Vec<String> = text
        .split("<message type=\"error\">")
        .skip(1)
        .filter_map(|m| m.split_once("</message>").map(|(t, _)| xml_unescape(t.trim())))
        .collect();
    let lines: Vec<String> = text
        .lines()
        .map(|l| l.trim().trim_start_matches(&format!("{program}: ")).to_string())
        .filter(|l| l.starts_with("E: ") || l.starts_with("Error") || l.starts_with("error:") || l.starts_with("No match"))
        .map(|l| l.trim_start_matches("error: ").to_string())
        .collect();
    let found = if xml.is_empty() { lines } else { xml };
    if found.is_empty() { tail(text) } else { found.join("; ") }
}

/// What `action` would do, from the package manager's dry run. Unknown
/// packages fail here, before anyone is asked.
pub(super) fn dry_run(policy: &Policy, m: Manager, action: Action, packages: &[String]) -> Result<Plan> {
    let argv = command(m, action, packages, true);
    match m {
        Manager::Apt => {
            let refs: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
            let text = exec::output("apt-get", &refs).map_err(|e| anyhow::anyhow!("{}", dry_run_error("apt-get", &format!("{e:#}"))))?;
            Ok(parse_apt_plan(&text))
        }
        Manager::Dnf => {
            let (done, _) = run_as_root(policy, m, argv)?;
            // --assumeno ends in "Operation aborted" and exit 1; anything else failed.
            let all = format!("{}\n{}", done.stdout, done.stderr);
            let nothing = all.contains("Nothing to do");
            ensure!(done.success() || nothing || all.contains("Operation aborted"), "{}", dry_run_error("dnf", &all));
            Ok(parse_dnf_transaction(&done.stdout))
        }
        Manager::Zypper => {
            let (done, _) = run_as_root(policy, m, argv)?;
            // 100-103: updates or patches pending, a reboot or restart needed.
            let ok = done.success() || matches!(done.status.code(), Some(100..=103));
            ensure!(ok, "{}", dry_run_error("zypper", &format!("{}\n{}", done.stdout, done.stderr)));
            Ok(parse_zypper_plan(&done.stdout))
        }
        Manager::Pacman => pacman_plan(m, action, &argv),
    }
}

fn pacman_plan(m: Manager, action: Action, argv: &[String]) -> Result<Plan> {
    let refs: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    let done = run_quiet("pacman", &refs, 120)?;
    ensure!(done.success(), "{}", dry_run_error("pacman", &format!("{}\n{}", done.stdout, done.stderr)));
    let targets: Vec<(String, String)> = done
        .stdout
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect();
    let mut plan = Plan::default();
    match action {
        Action::Remove => plan.remove = targets,
        Action::Install | Action::Restore => plan.install = targets,
        Action::Upgrade { .. } => {
            let upgrades = pending(m)?;
            let have: BTreeMap<String, String> = exec::output("pacman", &["-Q"])?
                .lines()
                .filter_map(|l| l.split_once(' '))
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect();
            let new: Vec<(String, String)> = targets.into_iter().filter(|(n, _)| !have.contains_key(n)).collect();
            // --noconfirm accepts "Replace orion-old with orion-new?": a removal.
            if !new.is_empty() {
                let mut args = pacman_db_args();
                args.push("-Si".into());
                args.push("--".into());
                args.extend(new.iter().map(|(n, _)| n.clone()));
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                let replaces = parse_pacman_replaces(&run_quiet("pacman", &refs, 60)?.stdout);
                for old in replaces.values().flatten() {
                    if let Some(version) = have.get(old) {
                        plan.remove.push((old.clone(), version.clone()));
                    }
                }
            }
            plan.upgrade = upgrades;
            plan.install = new;
        }
    }
    Ok(plan)
}

/// The privileged helper's check of a package manager command (`env
/// DEBIAN_FRONTEND=noninteractive apt-get …`, `dnf …`, `zypper …`, `pacman
/// …`): it must be exactly one [`command`] builds, on valid package names,
/// and a real change must not remove a package the helper's policy
/// protects or the system needs, by the manager's own dry run, as root.
pub(super) fn helper_check(policy: &Policy, program: &str, args: &[String]) -> Result<()> {
    let (m, argv): (Manager, Vec<String>) = match program {
        "env" => (Manager::Apt, args.to_vec()),
        "dnf" => (Manager::Dnf, std::iter::once(program.to_string()).chain(args.iter().cloned()).collect()),
        "zypper" => (Manager::Zypper, std::iter::once(program.to_string()).chain(args.iter().cloned()).collect()),
        "pacman" => (Manager::Pacman, std::iter::once(program.to_string()).chain(args.iter().cloned()).collect()),
        _ => bail!("{program} isn't a package manager Orion drives"),
    };
    let refresh: &[&str] = match m {
        Manager::Apt => &["DEBIAN_FRONTEND=noninteractive", "apt-get", "update", "-q"],
        Manager::Dnf => &["dnf", "makecache", "-q"],
        Manager::Zypper => &["zypper", "-n", "-q", "refresh"],
        Manager::Pacman => &[],
    };
    if !refresh.is_empty() && argv == refresh {
        return Ok(());
    }
    let packages: Vec<String> = argv.iter().position(|a| a == "--").map(|i| argv[i + 1..].to_vec()).unwrap_or_default();
    ensure!(packages.len() <= 200, "name at most 200 packages");
    let names_ok = packages.iter().all(|p| package_name(p).is_ok());
    let restore_ok = packages.iter().all(|p| super::changes::restore_spec(m, p).is_ok());
    ensure!(names_ok || restore_ok, "{} aren't package names", packages.join(" "));
    let up = |security_only, full| Action::Upgrade { security_only, full };
    let actions = [Action::Install, Action::Remove, up(false, false), up(true, false), up(false, true), up(true, true), Action::Restore];
    for action in actions {
        let fits = if matches!(action, Action::Restore) { restore_ok } else { names_ok };
        if !fits {
            continue;
        }
        for dry in [true, false] {
            if command(m, action, &packages, dry) != argv {
                continue;
            }
            if !dry {
                // The helper's own dry run, and its own protected list. Any
                // other removal was the asker's choice (`full`).
                let plan = dry_run(policy, m, action, &packages)?;
                guard_removals(policy, m, &plan, true)?;
            }
            return Ok(());
        }
    }
    bail!("that isn't a command Orion makes for {}", m.name())
}

/// For the health watch: (pending updates, of them security fixes, a
/// kernel among them), from the lists as they are (no refresh).
pub(super) fn summary(policy: &Policy) -> Result<(usize, usize, bool)> {
    let m = manager(policy)?;
    let list = pending(m)?;
    Ok((list.len(), list.iter().filter(|u| u.security).count(), list.iter().any(|u| kernel_package(&u.name))))
}

/// For the health watch: why the machine needs a reboot, if it does.
pub(super) fn reboot_needed(policy: &Policy) -> Vec<String> {
    let m = manager(policy).unwrap_or(Manager::Apt);
    let (stale, _) = stale_processes(Path::new("/proc"));
    reboot_reasons(m, &stale)
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
pub(super) fn unit_of(cgroup: &str) -> Option<(String, bool)> {
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
        // Orion itself is about to exit.
        if pid == std::process::id() {
            continue;
        }
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
fn reboot_reasons(m: Manager, stale: &[Stale]) -> Vec<String> {
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
    if why.is_empty() {
        // Each manager's own verdict on core packages updated since boot.
        let asked = match m {
            Manager::Dnf => run_quiet("dnf", &["-q", "-C", "needs-restarting", "-r"], 60).is_ok_and(|d| d.status.code() == Some(1)),
            Manager::Zypper => run_quiet("zypper", &["-n", "needs-rebooting"], 60).is_ok_and(|d| d.status.code() == Some(102)),
            Manager::Apt | Manager::Pacman => false,
        };
        if asked {
            why.push(format!("{} reports core packages updated since boot", m.name()));
        }
    }
    why.dedup();
    why
}

/// What still runs old code: services to restart, other processes, reboot.
fn after_effects(m: Manager) -> Value {
    let (stale, unreadable) = stale_processes(Path::new("/proc"));
    let reboot = reboot_reasons(m, &stale);
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

/// Fold `more` into `out`, joining the two notes if both have one.
fn merge(out: &mut Value, mut more: Value) {
    if let (Some(a), Some(b)) = (out.get("note").and_then(Value::as_str), more.get("note").and_then(Value::as_str)) {
        more["note"] = json!(format!("{a}; {b}"));
    }
    if let (Value::Object(out), Value::Object(more)) = (out, more) {
        out.extend(more);
    }
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
        let mut notes = Vec::new();
        if m == Manager::Pacman {
            notes.push("Arch publishes no security metadata, so no update is marked as a security fix");
        }
        if m == Manager::Zypper && tumbleweed() {
            notes.push("on Tumbleweed, update_apply runs a full distribution upgrade, which may change more than this list");
        }
        if !notes.is_empty() {
            out["note"] = json!(notes.join("; "));
        }
        if let Some(e) = refresh_error {
            out["refresh_error"] = json!(e);
        }
        merge(&mut out, after_effects(m));
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
    action: Action,
    /// Only these packages (empty: everything).
    only: Vec<String>,
    plan: Plan,
}

/// Refuse a removal of anything protected or essential, and without
/// `full`, any removal at all.
pub(super) fn guard_removals(policy: &Policy, m: Manager, plan: &Plan, full: bool) -> Result<()> {
    for (name, _) in &plan.remove {
        let bare = name.split(':').next().unwrap_or(name);
        if let Some(p) = policy.packages.protected.iter().find(|p| glob_match(p, bare)) {
            bail!("this update would remove {name}, which is protected (packages.protected: {p:?})");
        }
        if m == Manager::Apt && essential(name) {
            bail!("this update would remove {name}, which the system needs");
        }
    }
    if !full && !plan.remove.is_empty() {
        let names: Vec<&str> = plan.remove.iter().map(|(n, _)| n.as_str()).collect();
        bail!("this update would remove {} (replaced or obsoleted); set full to allow that", names.join(", "));
    }
    Ok(())
}

fn prepare(policy: &Policy, args: &Value) -> Result<Prepared> {
    let a: ApplyArgs = parse_args(args.clone())?;
    ensure!(a.packages.len() <= 200, "name at most 200 packages");
    for p in &a.packages {
        package_name(p)?;
    }
    ensure!(a.packages.is_empty() || !a.security_only, "give either packages or security_only, not both");
    let m = manager(policy)?;
    if m == Manager::Pacman {
        ensure!(a.packages.is_empty(), "Arch does not support upgrading only some packages; upgrade everything");
        ensure!(!a.security_only, "Arch publishes no security metadata; upgrade everything");
    }
    let mut only = a.packages;
    let mut action = Action::Upgrade { security_only: a.security_only, full: a.full };
    if a.security_only && m == Manager::Apt {
        // apt has no security-only upgrade: name the packages instead.
        only = pending(m)?.into_iter().filter(|u| u.security).map(|u| u.name).collect();
        if only.is_empty() {
            return Ok(Prepared { m, action, only, plan: Plan::default() });
        }
        action = Action::Upgrade { security_only: false, full: a.full };
    }
    let plan = dry_run(policy, m, action, &only)?;
    guard_removals(policy, m, &plan, a.full)?;
    Ok(Prepared { m, action, only, plan })
}

/// The ok exit codes of a real run: zypper uses 100-103 for "done, and a
/// reboot (102) or another run (103, after it updated itself) is due".
fn finished_ok(m: Manager, done: &exec::Finished) -> bool {
    done.success() || (m == Manager::Zypper && matches!(done.status.code(), Some(100..=103)))
}

impl Tool for UpdateApply {
    fn name(&self) -> &'static str {
        "update_apply"
    }

    fn description(&self) -> &'static str {
        "Install system updates, as root and outside the sandbox: everything, only security fixes \
         (security_only), or only the named packages. Never removes packages unless full is set, and even \
         then never one the system needs or the policy protects. Returns what was upgraded and whether \
         services need a restart or the machine a reboot. Use update_check first."
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
                                          Default false: on apt such upgrades are kept back; elsewhere the \
                                          update is refused, naming what it would remove." }
            },
            "additionalProperties": false
        })
    }

    fn preapproval(&self, policy: &Policy, args: &Value) -> Preapproval {
        let a: ApplyArgs = match parse_args(args.clone()) {
            Ok(a) => a,
            Err(e) => return Preapproval::Refused(format!("{e:#}")),
        };
        // packages.updates is what may run without a person approving it.
        // With update_apply on "ask", anything else is asked about; on
        // "allow", where nobody is asked, anything else is refused.
        let ask = "set [tools] update_apply = \"ask\" to be asked";
        match policy.packages.updates.as_str() {
            "full" => Preapproval::Granted,
            "all" | "security" | "ask" if a.full => Preapproval::Refused(format!(
                "an update with full may remove packages, so a person must approve it unless packages.updates = \"full\" ({ask})"
            )),
            "all" => Preapproval::Granted,
            "security" if a.security_only => Preapproval::Granted,
            "security" => Preapproval::Refused(format!(
                "packages.updates = \"security\" lets only security_only updates run without approval ({ask})"
            )),
            "ask" => Preapproval::Refused(format!(
                "packages.updates = \"ask\": updates need approval (allow some with \"security\" or \"all\", or {ask})"
            )),
            other => Preapproval::Refused(format!("packages.updates must be ask, security, all or full, not {other:?}")),
        }
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        super::changes::precheck(policy)?;
        prepare(policy, args).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let Prepared { m, action, only, mut plan } = prepare(policy, &args)?;
        if plan.is_empty() {
            return Ok(json!({ "ok": true, "manager": m.name(), "upgraded": [], "kept_back": plan.kept_back,
                              "message": "nothing to update" }));
        }
        // Mark security fixes where the dry run doesn't say (dnf, zypper).
        let security = security_names(m);
        for u in &mut plan.upgrade {
            u.security |= security.contains(&u.name);
        }
        let tracker = super::changes::begin(policy, m, "update_apply", None)?;
        let (done, elevated) = run_as_root(policy, m, command(m, action, &only, false))?;
        let ok = finished_ok(m, &done);
        let pairs = |list: &[(String, String)]| list.iter().map(|(n, v)| json!({ "name": n, "version": v })).collect::<Vec<_>>();
        let mut out = json!({
            "ok": ok,
            "manager": m.name(),
            "elevated": elevated,
            "duration_secs": done.duration.as_secs(),
            "kept_back": plan.kept_back,
            "output": tail(&done.stdout),
        });
        if ok {
            out["upgraded"] = json!(plan.upgrade.iter().map(Update::json).collect::<Vec<_>>());
            out["installed"] = json!(pairs(&plan.install));
            out["removed"] = json!(pairs(&plan.remove));
            if m == Manager::Zypper && done.status.code() == Some(103) {
                out["message"] = json!("zypper updated itself first; run update_apply again for the rest");
            }
        } else {
            out["error"] = json!(complaint(&done));
        }
        merge(&mut out, after_effects(m));
        tracker.finish(policy, ok, &mut out);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Orion;

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

        // dnf4, then dnf5 (as Fedora 44 prints it).
        let dnf4 = parse_dnf_security("FEDORA-2024-1a2b3c Important/Sec. curl-8.6.0-1.fc40.x86_64\n");
        let dnf5 = parse_dnf_security(
            "Name         Type     Severity                 Package              Issued\n\
             ORION-2026-1 security Important orion-sec-1.1-1.noarch 2026-10-01 00:00:00\n",
        );
        assert_eq!(dnf4, BTreeSet::from(["curl".to_string()]));
        assert_eq!(dnf5, BTreeSet::from(["orion-sec".to_string()]));
        let dnf = parse_dnf_updates(
            "Upgrades\ncurl.x86_64        8.6.0-1.fc40       updates\nkernel-core.x86_64 6.9.4-200.fc40 updates\n\
             Obsoleting Packages\ngrub2-tools.x86_64 1:2.06-1 updates\n",
            &dnf4,
        );
        assert_eq!(dnf.len(), 2);
        assert!(dnf[0].security && !dnf[1].security);
        assert!(kernel_package(&dnf[1].name));

        let zypper = parse_zypper_updates(
            r#"<update-list><update kind="package" name="orion-sec" edition="1.1-1" arch="noarch" edition-old="1.0-1"><summary>x</summary><source url="file:/t/repo" alias="orion-test"/></update><update kind="patch" name="P-1" edition="1"/></update-list>"#,
            &BTreeSet::from(["orion-sec".to_string()]),
        );
        assert_eq!(zypper.len(), 1);
        assert_eq!((zypper[0].from.as_deref(), zypper[0].to.as_str(), zypper[0].origin.as_str()), (Some("1.0-1"), "1.1-1", "orion-test"));
        assert!(zypper[0].security);
        let fixed = parse_zypper_patch_packages(
            "Name        : ORION-2026-1\nProvides    : patch:ORION-2026-1 = 1\nConflicts   : orion-sec < 1.1-1\n\
             \x20             libfoo1.x86_64 < 2.0\nSummary     : x\n",
        );
        assert_eq!(fixed, BTreeSet::from(["orion-sec".to_string(), "libfoo1".to_string()]));

        let pac = parse_pacman_updates("curl 8.5.0-1 -> 8.6.0-1\nlinux 6.9.1.arch1-1 -> 6.9.2.arch1-1\n");
        assert_eq!(pac[1], Update { name: "linux".into(), from: Some("6.9.1.arch1-1".into()), to: "6.9.2.arch1-1".into(), ..Default::default() });
        let replaces = parse_pacman_replaces("Name            : orion-new\nReplaces        : orion-old>=1 other\n\nName            : x\nReplaces        : None\n");
        assert_eq!(replaces["orion-new"], ["orion-old", "other"]);
        assert!(replaces["x"].is_empty());
    }

    #[test]
    fn parses_dry_runs() {
        let plan = parse_apt_plan(
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

        // dnf5, as Fedora 44 prints it: orion-new obsoletes orion-old.
        let dnf5 = parse_dnf_transaction(
            "Updating and loading repositories:\nRepositories loaded.\n\
             Package                 Arch   Version Repository      Size\n\
             Upgrading:\n \
             orion-demo             noarch 0:2.0-1 orion-test   4.0   B\n   \
               replacing orion-demo noarch 0:1.0-1 <unknown>    4.0   B\n\
             Installing:\n \
             orion-new              noarch 0:2.0-1 orion-test   4.0   B\n   \
               replacing orion-old  noarch 0:1.0-1 <unknown>    4.0   B\n\
             \nTransaction Summary:\n Installing:         1 package\n",
        );
        assert_eq!(dnf5.upgrade.len(), 1);
        assert_eq!((dnf5.upgrade[0].from.as_deref(), dnf5.upgrade[0].to.as_str()), (Some("1.0-1"), "2.0-1"));
        assert_eq!(dnf5.install, [("orion-new".into(), "2.0-1".into())]);
        assert_eq!(dnf5.remove, [("orion-old".into(), "1.0-1".into())]);
        // dnf4.
        let dnf4 = parse_dnf_transaction(
            " Package        Arch     Version        Repository   Size\n\
             ==========\nUpgrading:\n curl           x86_64   8.6.0-1.fc40   updates     300 k\n\
             Installing:\n orion-new      noarch   2.0-1          test          6 k\n     \
             replacing  orion-old.noarch 1.0-1\n\
             Removing dependent packages:\n foo            x86_64   1.0-1          @System      10 k\n\
             \nTransaction Summary\n",
        );
        assert_eq!(dnf4.upgrade[0].name, "curl");
        assert_eq!(dnf4.remove, [("orion-old".into(), "1.0-1".into()), ("foo".into(), "1.0-1".into())]);

        let zypper = parse_zypper_plan(
            r#"<stream><install-summary packages-to-change="4"><to-upgrade>
<solvable type="package" name="orion-sec" edition="1.1-1" arch="noarch" repository="orion-test" edition-old="1.0-1" arch-old="noarch" summary="x">
<description>Test package.</description></solvable>
</to-upgrade><to-install>
<solvable type="package" name="orion-new" edition="2.0-1" arch="noarch" repository="orion-test" summary="x">
<solvable type="patch" name="ORION-2026-1" edition="1" arch="noarch" repository="orion-test" summary="x"/>
</to-install><to-remove>
<solvable type="package" name="orion-old" edition="1.0-1" arch="noarch" repository="@System" summary="x &amp; y">
</to-remove></install-summary></stream>"#,
        );
        assert_eq!(zypper.upgrade[0].from.as_deref(), Some("1.0-1"));
        assert_eq!(zypper.install, [("orion-new".into(), "2.0-1".into())]);
        assert_eq!(zypper.remove, [("orion-old".into(), "1.0-1".into())]);
    }

    #[test]
    fn builds_commands() {
        let pkgs = ["curl".to_string()];
        let up = |security_only, full| Action::Upgrade { security_only, full };
        assert_eq!(command(Manager::Apt, up(false, false), &[], true), ["apt-get", "-s", "upgrade", "--with-new-pkgs"]);
        assert_eq!(command(Manager::Apt, up(false, false), &pkgs, true), ["apt-get", "-s", "install", "--only-upgrade", "--", "curl"]);
        assert_eq!(command(Manager::Dnf, up(true, false), &[], true), ["dnf", "--assumeno", "upgrade", "--security"]);
        assert_eq!(command(Manager::Dnf, Action::Remove, &pkgs, false), ["dnf", "-y", "-q", "remove", "--", "curl"]);
        assert_eq!(
            command(Manager::Zypper, up(true, false), &[], true),
            ["zypper", "-n", "--xmlout", "patch", "--category", "security", "--dry-run"]
        );
        assert_eq!(command(Manager::Zypper, up(false, false), &pkgs, false), ["zypper", "-n", "update", "--", "curl"]);
        assert_eq!(command(Manager::Pacman, Action::Install, &pkgs, false), ["pacman", "-S", "--needed", "--noconfirm", "--", "curl"]);
        assert_eq!(command(Manager::Pacman, up(false, false), &[], false), ["pacman", "-Syu", "--noconfirm"]);
    }

    #[test]
    fn helper_takes_only_orions_package_commands() {
        let p = Policy::from_toml("").unwrap();
        let v = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let check = |program: &str, a: &[&str]| helper_check(&p, program, &v(a));
        // What Orion sends: refreshing the lists, and dry runs.
        assert!(check("env", &["DEBIAN_FRONTEND=noninteractive", "apt-get", "update", "-q"]).is_ok());
        assert!(check("dnf", &["makecache", "-q"]).is_ok());
        assert!(check("dnf", &["--assumeno", "upgrade", "--security"]).is_ok());
        assert!(check("zypper", &["-n", "--xmlout", "install", "--dry-run", "--", "jq"]).is_ok());
        // Anything else: an option that runs a command, another variable, a
        // file instead of a package, a program that isn't a package manager.
        let refused = [
            ("env", v(&["DEBIAN_FRONTEND=noninteractive", "apt-get", "-o", "APT::Update::Pre-Invoke::=sh", "update", "-q"])),
            ("env", v(&["LD_PRELOAD=/tmp/x.so", "apt-get", "update", "-q"])),
            ("env", v(&["DEBIAN_FRONTEND=noninteractive", "sh", "-c", "id"])),
            ("env", v(&["DEBIAN_FRONTEND=noninteractive", "apt-get", "-y", "-q", "-o", "Dpkg::Options::=--force-confold", "install", "--", "./evil.deb"])),
            ("dnf", v(&["-y", "-q", "--setopt=pluginpath=/tmp", "install", "--", "jq"])),
            ("pacman", v(&["-U", "--noconfirm", "--", "/tmp/evil.pkg.tar.zst"])),
            ("zypper", v(&["-n", "addrepo", "http://evil.example/", "evil"])),
            ("apt-get", v(&["update"])),
        ];
        for (program, args) in refused {
            assert!(helper_check(&p, program, &args).is_err(), "{program} {args:?}");
        }
        // A real change is dry-run by the helper itself, against its policy.
        if exec::installed("apt-get") && exec::installed("dpkg-query") {
            let real = |verb: &str, pkg: &str| {
                command(Manager::Apt, if verb == "remove" { Action::Remove } else { Action::Install }, &[pkg.to_string()], false)
            };
            // Refused by the helper, or before that by apt's own dry run,
            // depending on the apt version.
            assert!(helper_check(&p, "env", &real("remove", "coreutils")).is_err());
            // Protected by the helper's policy, even if the asker's says otherwise.
            let strict = Policy::from_toml("[packages]\nprotected = [\"curl\"]\n").unwrap();
            let err = helper_check(&strict, "env", &real("remove", "curl")).unwrap_err();
            assert!(format!("{err:#}").contains("protected"), "{err:#}");
        }
    }

    #[test]
    fn guards_removals() {
        let p = Policy::from_toml("").unwrap();
        let plan = |names: &[&str]| Plan { remove: names.iter().map(|n| (n.to_string(), String::new())).collect(), ..Default::default() };
        assert!(guard_removals(&p, Manager::Dnf, &plan(&[]), false).is_ok());
        let err = guard_removals(&p, Manager::Dnf, &plan(&["orion-old"]), false).unwrap_err();
        assert!(format!("{err:#}").contains("set full"), "{err:#}");
        assert!(guard_removals(&p, Manager::Dnf, &plan(&["orion-old"]), true).is_ok());
        let err = guard_removals(&p, Manager::Dnf, &plan(&["systemd-udev"]), true).unwrap_err();
        assert!(format!("{err:#}").contains("protected"), "{err:#}");
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
        assert!(granted(&p("full"), json!({ "full": true })));
        assert!(matches!(UpdateApply.preapproval(&p("sometimes"), &json!({})), Preapproval::Refused(_)));

        // On "allow", nobody is asked: packages.updates is a hard limit.
        let allowed = |mode: &str| {
            Orion::new(Policy::from_toml(&format!("[tools]\nupdate_apply = \"allow\"\n[packages]\nupdates = \"{mode}\"\n[audit]\nenabled = false\n")).unwrap())
        };
        let refused = |o: &Orion, a: Value| match o.call("update_apply", a, None) {
            Err(crate::CallError::Denied(why)) => why,
            other => panic!("expected a refusal, got {other:?}"),
        };
        assert!(refused(&allowed("security"), json!({ "packages": ["curl"] })).contains("only security_only"));
        assert!(refused(&allowed("all"), json!({ "full": true })).contains("a person must approve"));
        assert!(refused(&allowed("ask"), json!({})).contains("updates need approval"));
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
