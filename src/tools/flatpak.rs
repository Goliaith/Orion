//! Flatpak apps: searching the remotes (Flathub), and installing or removing
//! apps within `[flatpak]`.
//!
//! Many desktop apps (Discord, Spotify, Steam, …) are on Flathub and not in
//! the distribution's repositories. Like `pkg_install`, installs run outside
//! the sandbox, since they download from the remote. System-wide installs
//! are tried as you first (polkit lets desktop admins install apps on most
//! distributions), then as root through `[admin] elevate`.

use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::exec::{self, glob_match};
use super::packages::{complaint, tail};
use crate::policy::Policy;
use crate::tool::{parse_args, Preapproval, Risk, Tool};

/// An application id, as Flatpak spells them: `com.discordapp.Discord`.
/// At least three dot-separated parts; anything that could be read as an
/// option is refused.
pub(super) fn app_id(raw: &str) -> Result<&str> {
    let parts: Vec<&str> = raw.split('.').collect();
    let ok = raw.len() <= 255
        && parts.len() >= 3
        && parts.iter().all(|p| {
            p.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        });
    ensure!(ok, "{raw:?} is not a Flatpak app id (like com.discordapp.Discord)");
    Ok(raw)
}

/// A remote's name: `flathub`, `fedora`.
pub(super) fn remote_name(raw: &str) -> Result<&str> {
    let ok = !raw.is_empty()
        && raw.len() <= 100
        && raw.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && raw.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    ensure!(ok, "{raw:?} is not a Flatpak remote name");
    Ok(raw)
}

#[derive(Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(rename_all = "lowercase")]
enum Scope {
    /// For everyone on the machine, as Discover and GNOME Software install.
    #[default]
    System,
    /// Only for the user Orion runs as; needs no root.
    User,
}

impl Scope {
    fn flag(self) -> &'static str {
        match self {
            Scope::System => "--system",
            Scope::User => "--user",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Scope::System => "system",
            Scope::User => "user",
        }
    }
}

fn schema_scope() -> Value {
    json!({ "type": "string", "enum": ["system", "user"], "default": "system",
            "description": "system: for everyone, as the app store installs. user: only for the user Orion runs as." })
}

fn require_flatpak() -> Result<()> {
    ensure!(exec::installed("flatpak"), "Flatpak isn't installed here; pkg_install can install the flatpak package");
    Ok(())
}

/// The remotes configured in `scope`.
fn remotes(scope: Scope) -> Result<Vec<String>> {
    let text = exec::output("flatpak", &["remotes", scope.flag(), "--columns=name"])?;
    Ok(text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
}

/// The installed apps: (id, installation, version).
fn installed_apps() -> Vec<(String, String, String)> {
    exec::output("flatpak", &["list", "--app", "--columns=application,installation,version"])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            Some((f.first()?.trim().to_string(), f.get(1).unwrap_or(&"").trim().to_string(), f.get(2).unwrap_or(&"").trim().to_string()))
        })
        .filter(|(id, _, _)| !id.is_empty())
        .collect()
}

fn installed_in(app: &str, scope: Scope) -> Option<String> {
    installed_apps().into_iter().find(|(id, inst, _)| id == app && inst == scope.name()).map(|(_, _, v)| v)
}

/// `flatpak search` rows: application, name, description, version, remotes.
fn parse_search(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').map(str::trim).collect();
            let id = *f.first()?;
            app_id(id).ok()?;
            Some(json!({
                "app": id,
                "name": f.get(1).unwrap_or(&""),
                "summary": f.get(2).unwrap_or(&""),
                "version": f.get(3).unwrap_or(&""),
                "remotes": f.get(4).map(|r| r.split(',').map(str::trim).filter(|r| !r.is_empty()).collect::<Vec<_>>()).unwrap_or_default(),
            }))
        })
        .collect()
}

pub struct FlatpakSearch;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    25
}

impl Tool for FlatpakSearch {
    fn name(&self) -> &'static str {
        "flatpak_search"
    }

    fn description(&self) -> &'static str {
        "Search the Flatpak remotes (Flathub) for apps by name and description: app id, name, summary, \
         version, which remotes have it and whether it is installed. Use it when pkg_search doesn't find an \
         app; most desktop apps (Discord, Spotify, Steam, Zoom, …) are on Flathub."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1, "default": 25 }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn call(&self, _policy: &Policy, args: Value) -> Result<Value> {
        let a: SearchArgs = parse_args(args)?;
        let q = a.query.trim();
        ensure!(!q.is_empty() && !q.starts_with('-'), "{:?} is not a search", a.query);
        require_flatpak()?;
        let text = exec::output("flatpak", &["search", "--columns=application,name,description,version,remotes", "--", q])?;
        let mut hits = parse_search(&text);
        let ql = q.to_lowercase();
        let lower = |v: &Value, k: &str| v[k].as_str().unwrap_or_default().to_lowercase();
        hits.sort_by_key(|h| (lower(h, "name") != ql, !lower(h, "app").contains(&ql) && !lower(h, "name").contains(&ql)));
        let have = installed_apps();
        for h in &mut hits {
            let found = have.iter().find(|(id, _, _)| h["app"] == id.as_str());
            h["installed"] = json!(found.map(|(_, inst, version)| json!({ "scope": inst, "version": version })));
        }
        let total = hits.len();
        hits.truncate(a.limit);
        Ok(json!({ "matched": total, "truncated": total > a.limit, "apps": hits }))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallArgs {
    app: String,
    remote: Option<String>,
    #[serde(default)]
    scope: Scope,
}

/// Validate an install and return (app, remote) before anyone is asked.
fn check_install(policy: &Policy, a: &InstallArgs) -> Result<(String, String)> {
    let app = app_id(&a.app)?.to_string();
    let remote = match &a.remote {
        Some(r) => remote_name(r)?.to_string(),
        None => policy.flatpak.remotes.first().cloned().context("[flatpak] remotes is empty: no remote may be installed from")?,
    };
    ensure!(
        policy.flatpak.remotes.iter().any(|r| r == &remote),
        "{remote} is not in [flatpak] remotes ({}); no approval can install from it",
        policy.flatpak.remotes.join(", ")
    );
    require_flatpak()?;
    let configured = remotes(a.scope)?;
    if !configured.iter().any(|r| r == &remote) {
        let flathub = remote == "flathub";
        bail!(
            "there is no {} remote {remote}{}",
            a.scope.name(),
            if flathub {
                " (add it with: flatpak remote-add --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo)"
            } else {
                ""
            }
        );
    }
    if let Some(v) = installed_in(&app, a.scope) {
        bail!("{app} is already installed ({}{}); app_open starts it", a.scope.name(), if v.is_empty() { String::new() } else { format!(", {v}") });
    }
    // Unknown apps fail here, not after the approval.
    exec::output("flatpak", &["remote-info", a.scope.flag(), "--", &remote, &app])
        .with_context(|| format!("couldn't find {app} on {remote} (flatpak_search finds app ids)"))?;
    Ok((app, remote))
}

/// The command lines the helper runs as root; `helper_check` accepts only these.
fn install_argv(remote: &str, app: &str) -> Vec<String> {
    ["install", "--system", "--noninteractive", "-y", "--", remote, app].map(String::from).to_vec()
}

fn uninstall_argv(app: &str) -> Vec<String> {
    ["uninstall", "--system", "--noninteractive", "-y", "--", app].map(String::from).to_vec()
}

/// Run a system-wide `flatpak` command as you, and if polkit says no, as
/// root; user ones only as you. (finished, elevated).
fn run(policy: &Policy, scope: Scope, argv: Vec<String>) -> Result<(exec::Finished, bool)> {
    let timeout = Duration::from_secs(policy.packages.timeout_secs.max(30));
    let attempt = |how: Option<&exec::Elevation>, argv: &[String]| exec::run_as(how, "flatpak", argv, None, timeout, 256 << 10);
    let mut direct = argv.clone();
    if scope == Scope::User {
        direct[1] = "--user".into();
        return Ok((attempt(None, &direct)?, false));
    }
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return Ok((attempt(None, &argv)?, false));
    }
    let done = attempt(None, &argv)?;
    let e = done.stderr.to_lowercase();
    let denied = e.contains("not allowed for user") || e.contains("not authorized") || e.contains("authentication");
    if !done.success() && denied {
        if let Some(how) = exec::elevation(policy)? {
            return Ok((attempt(Some(&how), &argv)?, true));
        }
    }
    Ok((done, false))
}

pub struct FlatpakInstall;

impl Tool for FlatpakInstall {
    fn name(&self) -> &'static str {
        "flatpak_install"
    }

    fn description(&self) -> &'static str {
        "Install a Flatpak app (by app id, from flatpak_search) from a remote such as Flathub, outside the \
         sandbox and with network access. System-wide by default, as the app store installs. Unknown apps \
         and remotes are reported before anything runs. Afterwards app_open starts the app."
    }

    fn risk(&self) -> Risk {
        Risk::Execute
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "app": { "type": "string", "description": "The app id, like com.discordapp.Discord." },
                "remote": { "type": "string", "description": "Where from; the first of [flatpak] remotes (flathub) by default." },
                "scope": schema_scope()
            },
            "required": ["app"],
            "additionalProperties": false
        })
    }

    fn preapproval(&self, policy: &Policy, args: &Value) -> Preapproval {
        let app = args.get("app").and_then(Value::as_str).unwrap_or_default();
        if policy.flatpak.allowed.iter().any(|p| glob_match(p, app)) {
            Preapproval::Granted
        } else {
            Preapproval::Refused(format!("{app:?} is not in flatpak.allowed"))
        }
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        check_install(policy, &parse_args(args.clone())?).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: InstallArgs = parse_args(args)?;
        let (app, remote) = check_install(policy, &a)?;
        let (done, elevated) = run(policy, a.scope, install_argv(&remote, &app))?;
        let mut out = json!({
            "ok": done.success(),
            "app": app,
            "remote": remote,
            "scope": a.scope.name(),
            "elevated": elevated,
            "duration_secs": done.duration.as_secs(),
            "output": tail(&done.stdout),
        });
        if done.success() {
            out["version"] = json!(installed_in(&app, a.scope));
            out["next"] = json!(format!("app_open with app {app} starts it"));
        } else {
            out["error"] = json!(complaint(&done));
        }
        Ok(out)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveArgs {
    app: String,
    #[serde(default)]
    scope: Scope,
}

fn check_remove(a: &RemoveArgs) -> Result<String> {
    let app = app_id(&a.app)?.to_string();
    require_flatpak()?;
    ensure!(installed_in(&app, a.scope).is_some(), "{app} isn't installed ({})", a.scope.name());
    Ok(app)
}

pub struct FlatpakRemove;

impl Tool for FlatpakRemove {
    fn name(&self) -> &'static str {
        "flatpak_remove"
    }

    fn description(&self) -> &'static str {
        "Uninstall a Flatpak app by app id. Its data in ~/.var/app stays, so reinstalling it keeps your settings."
    }

    fn risk(&self) -> Risk {
        Risk::Execute
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "app": { "type": "string", "description": "The app id, like com.discordapp.Discord." },
                "scope": schema_scope()
            },
            "required": ["app"],
            "additionalProperties": false
        })
    }

    fn precheck(&self, _policy: &Policy, args: &Value) -> Result<()> {
        check_remove(&parse_args(args.clone())?).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: RemoveArgs = parse_args(args)?;
        let app = check_remove(&a)?;
        let (done, elevated) = run(policy, a.scope, uninstall_argv(&app))?;
        let mut out = json!({ "ok": done.success(), "app": app, "scope": a.scope.name(), "elevated": elevated, "output": tail(&done.stdout) });
        if !done.success() {
            out["error"] = json!(complaint(&done));
        }
        Ok(out)
    }
}

/// The privileged helper's check: only the install and uninstall lines
/// above, of valid ids, from a remote in the helper's own `[flatpak] remotes`.
pub(super) fn helper_check(policy: &Policy, args: &[String]) -> Result<()> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["install", ..] if a.len() == 7 => {
            let (remote, app) = (remote_name(a[5])?, app_id(a[6])?);
            ensure!(args == install_argv(remote, app), "that isn't a flatpak command Orion makes");
            ensure!(policy.flatpak.remotes.iter().any(|r| r == remote), "{remote} is not in the helper's [flatpak] remotes");
            Ok(())
        }
        ["uninstall", ..] if a.len() == 6 => {
            let app = app_id(a[5])?;
            ensure!(args == uninstall_argv(app), "that isn't a flatpak command Orion makes");
            Ok(())
        }
        _ => bail!("that isn't a flatpak command Orion makes"),
    }
}

/// `flatpak install`, `uninstall` and friends as `proc_run` would start
/// them: they need the network (and often root), so they go through the
/// tools instead. The tool to use, if `command args` is one.
pub(super) fn redirect(command: &str, args: &[String]) -> Option<&'static str> {
    let program = command.rsplit('/').next().unwrap_or(command);
    let verb = args.iter().find(|a| !a.starts_with('-')).map(String::as_str).unwrap_or_default();
    match (program, verb) {
        ("flatpak", "install") => Some("flatpak_install"),
        ("flatpak", "uninstall" | "remove") => Some("flatpak_remove"),
        ("dnf" | "dnf5" | "yum" | "apt" | "apt-get" | "zypper", "install" | "in") => Some("pkg_install"),
        ("dnf" | "dnf5" | "yum" | "apt" | "apt-get" | "zypper", "remove" | "erase" | "rm" | "autoremove") => Some("pkg_remove"),
        ("dnf" | "dnf5" | "yum" | "apt" | "apt-get" | "zypper", "upgrade" | "update" | "dist-upgrade" | "full-upgrade" | "dup" | "up") => {
            Some("update_apply")
        }
        ("pacman", _) if args.iter().any(|a| a.starts_with("-S") || a == "--sync") => Some("pkg_install or update_apply"),
        ("pacman", _) if args.iter().any(|a| a.starts_with("-R") || a == "--remove") => Some("pkg_remove"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn app_ids_and_remotes() {
        for ok in ["com.discordapp.Discord", "org.mozilla.firefox", "com.valvesoftware.Steam", "io.github.some_one.App-2"] {
            assert!(app_id(ok).is_ok(), "{ok}");
        }
        for bad in ["", "discord", "com.discord", "-y", "--system", "com..x", "com.x.1app", "com.x.a b", "com.x.$(id)", "../x.y.z"] {
            assert!(app_id(bad).is_err(), "{bad:?}");
        }
        assert!(remote_name("flathub").is_ok() && remote_name("flathub-beta").is_ok());
        assert!(remote_name("--user").is_err() && remote_name("").is_err() && remote_name("a/b").is_err());
    }

    #[test]
    fn parses_search() {
        let text = "Discord\tcom.discordapp.Discord\tx\n"; // a malformed row is skipped
        assert!(parse_search(text).is_empty());
        let text = "com.discordapp.Discord\tDiscord\tMessaging, voice and video\t0.0.70\tflathub\n\
                    com.github.x.Webcord\tWebCord\tA Discord client\t4.9\tflathub,flathub-beta\n";
        let hits = parse_search(text);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0]["app"], "com.discordapp.Discord");
        assert_eq!(hits[0]["name"], "Discord");
        assert_eq!(hits[1]["remotes"], json!(["flathub", "flathub-beta"]));
    }

    #[test]
    fn install_preapproval() {
        let p = Policy::from_toml("[flatpak]\nallowed = [\"com.discordapp.*\"]\n").unwrap();
        assert!(matches!(FlatpakInstall.preapproval(&p, &json!({ "app": "com.discordapp.Discord" })), Preapproval::Granted));
        assert!(matches!(FlatpakInstall.preapproval(&p, &json!({ "app": "com.spotify.Client" })), Preapproval::Refused(_)));
    }

    #[test]
    fn only_allowed_remotes() {
        let p = Policy::from_toml("").unwrap();
        let a = InstallArgs { app: "com.discordapp.Discord".into(), remote: Some("sketchy".into()), scope: Scope::System };
        let err = check_install(&p, &a).unwrap_err();
        assert!(format!("{err:#}").contains("not in [flatpak] remotes"), "{err:#}");
    }

    #[test]
    fn the_helper_runs_only_what_orion_makes() {
        let p = Policy::from_toml("").unwrap();
        assert!(helper_check(&p, &install_argv("flathub", "com.discordapp.Discord")).is_ok());
        assert!(helper_check(&p, &uninstall_argv("com.discordapp.Discord")).is_ok());
        for bad in [
            s(&["install", "--system", "--noninteractive", "-y", "--", "evil", "com.discordapp.Discord"]),
            s(&["install", "--system", "--noninteractive", "-y", "--", "flathub", "/tmp/x.flatpakref"]),
            s(&["install", "--system", "--noninteractive", "-y", "--from", "flathub", "https://x/y.flatpakref"]),
            s(&["remote-add", "evil", "https://evil.example/repo"]),
            s(&["run", "com.discordapp.Discord"]),
            s(&["uninstall", "--system", "--noninteractive", "-y", "--all", "x.y.z"]),
        ] {
            assert!(helper_check(&p, &bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn proc_run_installs_go_to_the_tools() {
        assert_eq!(redirect("flatpak", &s(&["install", "-y", "flathub", "com.discordapp.Discord"])), Some("flatpak_install"));
        assert_eq!(redirect("/usr/bin/flatpak", &s(&["--user", "uninstall", "x.y.z"])), Some("flatpak_remove"));
        assert_eq!(redirect("dnf", &s(&["install", "-y", "htop"])), Some("pkg_install"));
        assert_eq!(redirect("pacman", &s(&["-Syu"])), Some("pkg_install or update_apply"));
        assert_eq!(redirect("flatpak", &s(&["list", "--app"])), None);
        assert_eq!(redirect("dnf", &s(&["search", "discord"])), None);
        assert_eq!(redirect("ls", &s(&["install"])), None);
    }
}
