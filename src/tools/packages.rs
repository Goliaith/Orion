//! The system package manager (apt, dnf, zypper or pacman): what is installed, what
//! is available, and installing or removing packages within `[packages]`.
//!
//! Installs run outside the sandbox, as root (see `[admin] elevate`): a
//! package's install scripts can do anything, which is why `pkg_install`
//! is an `execute` tool.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, ensure, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::exec::{self, glob_match};
use super::updates::{command, dry_run, Action, Plan};
use crate::policy::Policy;
use crate::tool::{parse_args, Preapproval, Risk, Tool};

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Manager {
    Apt,
    Dnf,
    Zypper,
    Pacman,
}

impl Manager {
    pub(super) fn name(self) -> &'static str {
        match self {
            Manager::Apt => "apt",
            Manager::Dnf => "dnf",
            Manager::Zypper => "zypper",
            Manager::Pacman => "pacman",
        }
    }
}

pub(super) fn manager(policy: &Policy) -> Result<Manager> {
    Ok(match policy.packages.manager.as_str() {
        "apt" => Manager::Apt,
        "dnf" => Manager::Dnf,
        "zypper" => Manager::Zypper,
        "pacman" => Manager::Pacman,
        "auto" if exec::installed("apt-get") && exec::installed("dpkg-query") => Manager::Apt,
        "auto" if exec::installed("dnf") && exec::installed("rpm") => Manager::Dnf,
        "auto" if exec::installed("zypper") && exec::installed("rpm") => Manager::Zypper,
        "auto" if exec::installed("pacman") => Manager::Pacman,
        "auto" => bail!("no supported package manager (apt, dnf, zypper or pacman) is installed"),
        other => bail!("packages.manager must be auto, apt, dnf, zypper or pacman, not {other:?}"),
    })
}

/// Package names as the managers write them. Anything that could be read
/// as an option is refused.
pub(super) fn package_name(raw: &str) -> Result<&str> {
    let ok = !raw.is_empty()
        && raw.len() <= 200
        && raw.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && raw.chars().all(|c| c.is_ascii_alphanumeric() || "+-._:@".contains(c));
    ensure!(ok, "{raw:?} is not a package name");
    Ok(raw)
}

struct Installed {
    name: String,
    version: String,
    size_kib: Option<u64>,
    summary: String,
}

fn installed(m: Manager) -> Result<Vec<Installed>> {
    let text = match m {
        Manager::Apt => exec::output(
            "dpkg-query",
            &["-W", "-f", "${db:Status-Abbrev}\t${Package}\t${Version}\t${Installed-Size}\t${binary:Summary}\n"],
        )?,
        Manager::Dnf | Manager::Zypper => {
            exec::output("rpm", &["-qa", "--qf", "ii\t%{NAME}\t%{VERSION}-%{RELEASE}\t%{SIZE}\t%{SUMMARY}\n"])?
        }
        Manager::Pacman => exec::output("pacman", &["-Q"])?
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(n, v)| format!("ii\t{n}\t{v}\t\t\n"))
            .collect(),
    };
    Ok(text
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            // dpkg lists removed-but-configured packages too ("rc").
            if f.len() < 5 || !f[0].starts_with("ii") {
                return None;
            }
            let rpm = matches!(m, Manager::Dnf | Manager::Zypper);
            let size = f[3].parse::<u64>().ok().map(|n| if rpm { n / 1024 } else { n });
            Some(Installed { name: f[1].into(), version: f[2].into(), size_kib: size, summary: f[4].into() })
        })
        .collect())
}

/// `apt-cache search`, `dnf search`, `zypper --xmlout search` or `pacman -Ss`: (name, summary).
fn parse_search(m: Manager, text: &str) -> Vec<(String, String)> {
    match m {
        Manager::Apt => text
            .lines()
            .filter_map(|l| l.split_once(" - "))
            .map(|(n, s)| (n.trim().to_string(), s.trim().to_string()))
            .collect(),
        // dnf4: "jq.x86_64 : summary" under "=== … ===" headers;
        // dnf5: " jq.x86_64\tsummary" under "Matched fields: …".
        Manager::Dnf => text
            .lines()
            .filter(|l| !l.starts_with('='))
            .filter_map(|l| if l.starts_with(' ') { l.split_once('\t') } else { l.split_once(" : ") })
            .map(|(n, s)| {
                // name.arch
                let n = n.trim();
                let n = n.rsplit_once('.').map_or(n, |(name, _)| name);
                (n.to_string(), s.trim().to_string())
            })
            .collect(),
        Manager::Zypper => xml_elements(text, "solvable")
            .filter(|a| a.get("kind").is_none_or(|k| k == "package"))
            .filter_map(|mut a| Some((a.remove("name")?, a.remove("summary").unwrap_or_default())))
            .collect(),
        Manager::Pacman => {
            let mut out = Vec::new();
            let mut lines = text.lines().peekable();
            while let Some(head) = lines.next() {
                let Some(name) = head.split_whitespace().next().and_then(|n| n.split('/').nth(1)) else { continue };
                let summary = match lines.peek() {
                    Some(l) if l.starts_with(' ') => lines.next().unwrap_or_default().trim(),
                    _ => "",
                };
                out.push((name.to_string(), summary.to_string()));
            }
            out
        }
    }
}

/// The attributes of each `<tag …>` in zypper's XML output, unescaped.
/// Enough for zypper's flat, attribute-only records; not a general parser.
pub(super) fn xml_elements(text: &str, tag: &str) -> impl Iterator<Item = BTreeMap<String, String>> {
    let open = format!("<{tag} ");
    let found: Vec<_> = text.match_indices(&open).filter_map(|(i, _)| {
        let rest = &text[i + open.len()..];
        let end = rest.find('>')?;
        let mut attrs = BTreeMap::new();
        let mut body = rest[..end].trim_end_matches('/');
        while let Some((key, after)) = body.split_once("=\"") {
            let (value, next) = after.split_once('"')?;
            attrs.insert(key.trim().to_string(), xml_unescape(value));
            body = next;
        }
        Some(attrs)
    }).collect();
    found.into_iter()
}

pub(super) fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// `Key: value` lines (apt-cache show, dnf info, pacman -Si), with
/// continuation lines folded in.
pub(super) fn parse_fields(text: &str) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut last: Option<String> = None;
    for line in text.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(v) = last.as_ref().and_then(|k| out.get_mut(k)) {
                let t = line.trim().trim_start_matches(": ").trim();
                if !t.is_empty() && t != "." {
                    if !v.is_empty() {
                        v.push(' ');
                    }
                    v.push_str(t);
                }
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_string();
            if !k.is_empty() && !k.contains(' ') || matches!(k.as_str(), "Install Date" | "Installed Size" | "Installed size" | "Depends On") {
                out.entry(k.clone()).or_insert_with(|| v.trim().to_string());
                last = Some(k);
            }
        }
    }
    out
}

pub(super) fn essential(name: &str) -> bool {
    exec::output("dpkg-query", &["-W", "-f", "${Essential} ${Priority}", "--", name])
        .is_ok_and(|s| s.starts_with("yes") || s.ends_with("required"))
}

pub struct PkgList;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    filter: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    200
}

impl Tool for PkgList {
    fn name(&self) -> &'static str {
        "pkg_list"
    }

    fn description(&self) -> &'static str {
        "List installed system packages (apt, dnf or pacman) with versions, sizes and summaries. Filter by name."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "filter": { "type": "string", "description": "Case-insensitive substring of the package name." },
                "limit": { "type": "integer", "minimum": 1, "default": 200 }
            },
            "additionalProperties": false
        })
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: ListArgs = parse_args(args)?;
        let m = manager(policy)?;
        let all = installed(m)?;
        let total = all.len();
        let filter = a.filter.map(|f| f.to_lowercase());
        let matched: Vec<&Installed> =
            all.iter().filter(|p| filter.as_ref().is_none_or(|f| p.name.to_lowercase().contains(f))).collect();
        Ok(json!({
            "manager": m.name(),
            "installed": total,
            "matched": matched.len(),
            "truncated": matched.len() > a.limit,
            "packages": matched.iter().take(a.limit).map(|p| json!({
                "name": p.name, "version": p.version, "size_kib": p.size_kib, "summary": p.summary,
            })).collect::<Vec<_>>(),
        }))
    }
}

pub struct PkgSearch;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    #[serde(default = "default_search_limit")]
    limit: usize,
}

fn default_search_limit() -> usize {
    50
}

impl Tool for PkgSearch {
    fn name(&self) -> &'static str {
        "pkg_search"
    }

    fn description(&self) -> &'static str {
        "Search the package repositories by name and description. Exact name matches come first; \
         each result says whether it is installed."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1, "default": 50 }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: SearchArgs = parse_args(args)?;
        let q = a.query.trim();
        ensure!(!q.is_empty() && !q.starts_with('-'), "{:?} is not a search", a.query);
        let m = manager(policy)?;
        let text = match m {
            Manager::Apt => exec::output("apt-cache", &["search", "--", q])?,
            Manager::Dnf => exec::output("dnf", &["-q", "search", "--", q])?,
            // Exits 104 when nothing matches.
            Manager::Zypper => exec::output("zypper", &["-n", "--xmlout", "search", "--", q]).unwrap_or_default(),
            Manager::Pacman => exec::output("pacman", &["-Ss", "--", q]).unwrap_or_default(),
        };
        let mut hits = parse_search(m, &text);
        let ql = q.to_lowercase();
        hits.sort_by_key(|(n, _)| (n.to_lowercase() != ql, !n.to_lowercase().contains(&ql), n.len()));
        hits.dedup_by(|a, b| a.0 == b.0);
        let have: BTreeMap<String, String> = installed(m)?.into_iter().map(|p| (p.name, p.version)).collect();
        let total = hits.len();
        Ok(json!({
            "manager": m.name(),
            "matched": total,
            "truncated": total > a.limit,
            "packages": hits.into_iter().take(a.limit).map(|(name, summary)| json!({
                "installed": have.get(&name), "name": name, "summary": summary,
            })).collect::<Vec<_>>(),
        }))
    }
}

pub struct PkgInfo;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InfoArgs {
    name: String,
}

impl Tool for PkgInfo {
    fn name(&self) -> &'static str {
        "pkg_info"
    }

    fn description(&self) -> &'static str {
        "Describe one package: the installed version and the newest available one, its description, \
         size, dependencies and homepage."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: InfoArgs = parse_args(args)?;
        let name = package_name(&a.name)?;
        let m = manager(policy)?;
        let installed_version =
            installed(m)?.into_iter().find(|p| p.name == name).map(|p| p.version);
        let (f, candidate) = match m {
            Manager::Apt => {
                let show = exec::output("apt-cache", &["show", "--no-all-versions", "--", name]);
                let show = show.map_err(|_| anyhow::anyhow!("there is no package {name}"))?;
                let policy_text = exec::output("apt-cache", &["policy", "--", name]).unwrap_or_default();
                let candidate = policy_text
                    .lines()
                    .find_map(|l| l.trim().strip_prefix("Candidate:"))
                    .map(|v| v.trim().to_string())
                    .filter(|v| v != "(none)");
                (parse_fields(&show), candidate)
            }
            Manager::Dnf => {
                // The newest available version comes after the installed one, if any.
                let text = exec::output("dnf", &["-q", "info", "--", name])?;
                let f = parse_fields(text.split("Available").nth(1).unwrap_or(&text));
                let v = f.get("Version").map(|v| format!("{v}-{}", f.get("Release").cloned().unwrap_or_default()));
                (f, v)
            }
            Manager::Zypper => {
                let text = exec::output("zypper", &["-n", "-q", "info", "--", name])?;
                ensure!(!text.contains("not found"), "there is no package {name}");
                let f = parse_fields(&text);
                let v = f.get("Version").cloned();
                (f, v)
            }
            Manager::Pacman => {
                let text = exec::output("pacman", &["-Si", "--", name]).or_else(|_| exec::output("pacman", &["-Qi", "--", name]))?;
                let f = parse_fields(&text);
                let v = f.get("Version").cloned();
                (f, v)
            }
        };
        let field = |keys: &[&str]| keys.iter().find_map(|k| f.get(*k)).cloned();
        Ok(json!({
            "manager": m.name(),
            "name": name,
            "installed": installed_version,
            "available": candidate,
            "update": installed_version.is_some() && candidate.is_some() && installed_version != candidate,
            "summary": field(&["Description", "Summary"]),
            "size": f.get("Installed-Size").map(|kib| format!("{kib} KiB")).or_else(|| field(&["Installed Size", "Installed size", "Size"])),
            "depends": field(&["Depends", "Depends On"]),
            "homepage": field(&["Homepage", "URL"]),
            "section": field(&["Section", "Repository", "Repo"]),
        }))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangeArgs {
    packages: Vec<String>,
}

fn change_schema(verb: &str) -> Value {
    json!({
        "type": "object",
        "properties": {
            "packages": { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 50,
                          "description": format!("Package names to {verb}.") }
        },
        "required": ["packages"],
        "additionalProperties": false
    })
}

/// Validate the names and work out exactly what would change, from the
/// package manager's own dry run.
fn check_change(policy: &Policy, args: &Value, remove: bool) -> Result<(Manager, Vec<String>, Plan)> {
    let a: ChangeArgs = parse_args(args.clone())?;
    ensure!(!a.packages.is_empty() && a.packages.len() <= 50, "name 1 to 50 packages");
    for p in &a.packages {
        package_name(p)?;
    }
    let m = manager(policy)?;
    let guard = |name: &str| -> Result<()> {
        let bare = name.split(':').next().unwrap_or(name);
        if let Some(p) = policy.packages.protected.iter().find(|p| glob_match(p, bare)) {
            bail!("{name} is protected (packages.protected: {p:?}); no approval can remove it");
        }
        if m == Manager::Apt && essential(name) {
            bail!("{name} is essential: the system needs it");
        }
        Ok(())
    };
    if remove {
        a.packages.iter().try_for_each(|p| guard(p))?;
    }
    let plan = dry_run(policy, m, if remove { Action::Remove } else { Action::Install }, &a.packages)?;
    // Removing a package also removes what depends on it, and installing
    // one can replace another.
    for (name, _) in &plan.remove {
        guard(name).map_err(|e| anyhow::anyhow!("this would also remove {name}: {e:#}"))?;
    }
    Ok((m, a.packages, plan))
}

fn change(policy: &Policy, args: Value, remove: bool) -> Result<Value> {
    let (m, packages, plan) = check_change(policy, &args, remove)?;
    let argv = command(m, if remove { Action::Remove } else { Action::Install }, &packages, false);
    let (done, elevated) = run_as_root(policy, m, argv)?;
    let pairs = |list: Vec<(String, String)>| list.into_iter().map(|(name, version)| json!({ "name": name, "version": version })).collect::<Vec<_>>();
    let mut out = json!({
        "ok": done.success(),
        "manager": m.name(),
        "requested": packages,
        "elevated": elevated,
        "duration_secs": done.duration.as_secs(),
        "output": tail(&done.stdout),
    });
    if done.success() {
        // Upgraded dependencies count as installed.
        let mut installed = plan.install;
        installed.extend(plan.upgrade.into_iter().map(|u| (u.name, u.to)));
        if !installed.is_empty() || !remove {
            out["installed"] = json!(pairs(installed));
        }
        if !plan.remove.is_empty() || remove {
            out["removed"] = json!(pairs(plan.remove));
        }
    } else {
        out["error"] = json!(complaint(&done));
    }
    Ok(out)
}

/// Run a package manager command line as root, within `timeout_secs`.
/// For apt, `argv` starts with `DEBIAN_FRONTEND=noninteractive`: `env`
/// carries it through sudo, which resets the environment.
pub(super) fn run_as_root(policy: &Policy, m: Manager, mut argv: Vec<String>) -> Result<(exec::Finished, bool)> {
    let program = if m == Manager::Apt { "env".to_string() } else { argv.remove(0) };
    let prefix = exec::elevation(&policy.admin.elevate)?;
    let cmd = exec::command_as(prefix.as_deref(), &program, &argv);
    let done = exec::run(cmd, None, Duration::from_secs(policy.packages.timeout_secs.max(30)), 256 << 10)?;
    Ok((done, prefix.is_some()))
}

/// The last lines, each as it ended up (progress counters redraw with \r).
pub(super) fn tail(s: &str) -> String {
    let lines: Vec<&str> = s.lines().map(|l| l.rsplit('\r').next().unwrap_or(l)).collect();
    lines[lines.len().saturating_sub(20)..].join("\n")
}

/// Why a package manager run failed, with a hint when sudo wanted a password.
pub(super) fn complaint(done: &exec::Finished) -> String {
    let mut why = done.complaint();
    if why.contains("a password is required") {
        why.push_str(" (sudo needs a password here; see [admin] elevate in the Orion policy)");
    }
    tail(&why)
}

pub struct PkgInstall;

impl Tool for PkgInstall {
    fn name(&self) -> &'static str {
        "pkg_install"
    }

    fn description(&self) -> &'static str {
        "Install system packages with the package manager (apt, dnf or pacman), as root and outside the \
         sandbox. Unknown names are reported before anything runs. Returns every package that was \
         installed, dependencies included."
    }

    fn risk(&self) -> Risk {
        Risk::Execute
    }

    fn input_schema(&self) -> Value {
        change_schema("install")
    }

    fn preapproval(&self, policy: &Policy, args: &Value) -> Preapproval {
        let names: Vec<&str> = args
            .get("packages")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        match names.iter().find(|n| !policy.packages.allowed.iter().any(|p| glob_match(p, n))) {
            None if !names.is_empty() => Preapproval::Granted,
            None => Preapproval::Refused("no packages named".into()),
            Some(n) => Preapproval::Refused(format!("{n:?} is not in packages.allowed")),
        }
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        check_change(policy, args, false).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        change(policy, args, false)
    }
}

pub struct PkgRemove;

impl Tool for PkgRemove {
    fn name(&self) -> &'static str {
        "pkg_remove"
    }

    fn description(&self) -> &'static str {
        "Remove system packages. Refused, before anything runs, if it would take away a package the system \
         needs or one the policy protects (the kernel, systemd, sudo, ssh, …), including as a dependent."
    }

    fn risk(&self) -> Risk {
        Risk::Execute
    }

    fn input_schema(&self) -> Value {
        change_schema("remove")
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        check_change(policy, args, true).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        change(policy, args, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_names() {
        for ok in ["jq", "g++", "libc6:i386", "python3.12", "nvidia-driver-550"] {
            assert!(package_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-y", "--force", "a b", "x;y", "../x", "$(id)"] {
            assert!(package_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn parses_manager_output() {
        let apt = parse_search(Manager::Apt, "jq - lightweight JSON processor\njc - JSON CLI output\n");
        assert_eq!(apt[0], ("jq".into(), "lightweight JSON processor".into()));
        let dnf = parse_search(
            Manager::Dnf,
            "======= Name Exactly Matched: jq =======\njq.x86_64 : Command-line JSON processor\n",
        );
        assert_eq!(dnf, [("jq".into(), "Command-line JSON processor".into())]);
        let dnf5 = parse_search(Manager::Dnf, "Matched fields: name, summary\n jq.x86_64\tCommand-line JSON processor\n");
        assert_eq!(dnf5, dnf);
        let pac = parse_search(
            Manager::Pacman,
            "extra/jq 1.7.1-2 [installed]\n    Command-line JSON processor\ncore/jx 1-1\n    x\n",
        );
        assert_eq!(pac[0], ("jq".into(), "Command-line JSON processor".into()));
        assert_eq!(pac[1].0, "jx");
        let zyp = parse_search(
            Manager::Zypper,
            r#"<solvable status="installed" name="jq" summary="A &quot;JSON&quot; processor" kind="package"/><solvable status="not-installed" name="jq" summary="x" kind="srcpackage"/>"#,
        );
        assert_eq!(zyp, [("jq".into(), "A \"JSON\" processor".into())]);

        let f = parse_fields("Package: jq\nDescription: lightweight\n and flexible\n .\nDepends: libc6\n");
        assert_eq!(f["Description"], "lightweight and flexible");
        assert_eq!(f["Depends"], "libc6");
        let f = parse_fields("Name            : jq\nInstalled Size  : 1.2 MiB\nDepends On      : glibc\n");
        assert_eq!(f["Installed Size"], "1.2 MiB");
        assert_eq!(f["Depends On"], "glibc");
    }

    #[test]
    fn install_preapproval() {
        let p = Policy::from_toml("[packages]\nallowed = [\"python3-*\", \"jq\"]\n").unwrap();
        let args = |names: &[&str]| json!({ "packages": names });
        assert!(matches!(PkgInstall.preapproval(&p, &args(&["jq", "python3-numpy"])), Preapproval::Granted));
        assert!(matches!(PkgInstall.preapproval(&p, &args(&["jq", "nginx"])), Preapproval::Refused(_)));
        assert!(matches!(PkgInstall.preapproval(&p, &args(&[])), Preapproval::Refused(_)));
    }

    #[test]
    fn checks_against_the_real_manager() {
        if !exec::installed("dpkg-query") || !exec::installed("apt-get") {
            return;
        }
        let p = Policy::from_toml("").unwrap();
        let r = PkgList.call(&p, json!({ "filter": "dpkg" })).unwrap();
        assert!(r["packages"].as_array().unwrap().iter().any(|x| x["name"] == "dpkg"), "{r}");
        let r = PkgInfo.call(&p, json!({ "name": "dpkg" })).unwrap();
        assert!(r["installed"].is_string(), "{r}");

        // Unknown packages and protected or essential ones fail before any approval.
        let err = PkgInstall.precheck(&p, &json!({ "packages": ["orion-no-such-package"] })).unwrap_err();
        assert!(format!("{err:#}").contains("Unable to locate"), "{err:#}");
        assert!(PkgInstall.precheck(&p, &json!({ "packages": ["--yes"] })).is_err());
        let err = PkgRemove.precheck(&p, &json!({ "packages": ["dpkg"] })).unwrap_err();
        assert!(format!("{err:#}").contains("protected"), "{err:#}");
        let err = PkgRemove.precheck(&p, &json!({ "packages": ["coreutils"] })).unwrap_err();
        assert!(format!("{err:#}").contains("essential"), "{err:#}");
    }
}
