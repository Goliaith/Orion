//! systemd services: list them, read one's state and log, and start, stop,
//! restart or enable them within `[services]`.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::exec::{self, glob_match};
use crate::policy::Policy;
use crate::tool::{parse_args, Preapproval, Risk, Tool};

#[derive(Deserialize, Default, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
enum Scope {
    /// The machine's services.
    #[default]
    System,
    /// The services of the user Orion runs as.
    User,
}

impl Scope {
    fn flag(self) -> Option<&'static str> {
        match self {
            Scope::System => None,
            Scope::User => Some("--user"),
        }
    }
}

const SUFFIXES: [&str; 11] =
    [".service", ".socket", ".timer", ".path", ".target", ".mount", ".automount", ".swap", ".slice", ".scope", ".device"];
/// Unit types `svc_control` may act on.
const CONTROLLABLE: [&str; 4] = [".service", ".socket", ".timer", ".path"];

/// A unit name as systemd would take it, with `.service` added when the
/// name has no type. Anything that could be read as an option is refused.
fn unit_name(raw: &str) -> Result<String> {
    let raw = raw.trim();
    ensure!(!raw.is_empty() && raw.len() <= 255, "a unit name is 1 to 255 characters");
    ensure!(!raw.starts_with('-'), "{raw:?} is not a unit name");
    ensure!(
        raw.chars().all(|c| c.is_ascii_alphanumeric() || ":-_.@\\".contains(c)),
        "{raw:?} is not a unit name (letters, digits and :-_.@\\ only)"
    );
    Ok(if SUFFIXES.iter().any(|s| raw.ends_with(s)) { raw.to_string() } else { format!("{raw}.service") })
}

fn systemd_running(scope: Scope) -> Result<()> {
    if scope == Scope::System && !Path::new("/run/systemd/system").exists() {
        bail!("systemd is not running on this machine (or Orion runs in a container without it)");
    }
    Ok(())
}

fn systemctl(scope: Scope, args: &[&str]) -> Result<String> {
    systemd_running(scope)?;
    let mut all: Vec<&str> = scope.flag().into_iter().collect();
    all.extend(["--no-pager", "--plain", "--no-legend"]);
    all.extend(args);
    exec::output("systemctl", &all)
}

/// `systemctl list-units --plain --no-legend`: UNIT LOAD ACTIVE SUB DESCRIPTION.
fn parse_units(text: &str) -> Vec<(String, [String; 3], String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim_start_matches(['●', '*', ' ']);
            let mut f = l.split_whitespace();
            let unit = f.next()?.to_string();
            let states = [f.next()?.to_string(), f.next()?.to_string(), f.next()?.to_string()];
            Some((unit, states, f.collect::<Vec<_>>().join(" ")))
        })
        .collect()
}

/// `systemctl list-unit-files --plain --no-legend`: UNIT STATE [PRESET].
fn parse_unit_files(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((f.next()?.to_string(), f.next()?.to_string()))
        })
        .collect()
}

/// `systemctl show`: KEY=VALUE lines.
fn parse_show(text: &str) -> BTreeMap<String, String> {
    text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// Failed system units, for status displays; `None` without systemd.
pub fn failed_units() -> Option<Vec<String>> {
    let text = systemctl(Scope::System, &["list-units", "--state=failed"]).ok()?;
    Some(parse_units(&text).into_iter().map(|(unit, _, _)| unit).collect())
}

fn schema_scope() -> Value {
    json!({ "type": "string", "enum": ["system", "user"], "default": "system",
            "description": "The machine's services, or those of the user Orion runs as." })
}

pub struct SvcList;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default)]
    scope: Scope,
    filter: Option<String>,
    #[serde(default)]
    state: StateFilter,
    #[serde(default = "default_type", rename = "type")]
    kind: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
enum StateFilter {
    #[default]
    All,
    Running,
    Failed,
    Enabled,
}

fn default_type() -> String {
    "service".into()
}

fn default_limit() -> usize {
    200
}

impl Tool for SvcList {
    fn name(&self) -> &'static str {
        "svc_list"
    }

    fn description(&self) -> &'static str {
        "List systemd units (services by default): whether each is running, failed or stopped, whether it \
         starts at boot, and what it is. Filter by name or state."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": schema_scope(),
                "filter": { "type": "string", "description": "Case-insensitive substring of the name or description." },
                "state": { "type": "string", "enum": ["all", "running", "failed", "enabled"], "default": "all" },
                "type": { "type": "string", "enum": ["service", "timer", "socket", "path", "target", "mount"], "default": "service" },
                "limit": { "type": "integer", "minimum": 1, "default": 200 }
            },
            "additionalProperties": false
        })
    }

    fn call(&self, _policy: &Policy, args: Value) -> Result<Value> {
        let a: ListArgs = parse_args(args)?;
        ensure!(a.kind.chars().all(|c| c.is_ascii_lowercase()), "unknown unit type {:?}", a.kind);
        let kind = format!("--type={}", a.kind);
        let units = parse_units(&systemctl(a.scope, &["list-units", "--all", &kind])?);
        let files = parse_unit_files(&systemctl(a.scope, &["list-unit-files", &kind]).unwrap_or_default());

        let mut rows: BTreeMap<String, Value> = BTreeMap::new();
        for (unit, [load, active, sub], description) in units {
            let enabled = files.get(&unit).cloned();
            rows.insert(
                unit.clone(),
                json!({ "unit": unit, "description": description, "load": load, "active": active, "sub": sub, "enabled": enabled }),
            );
        }
        // Installed but never loaded (stopped and not enabled, usually).
        for (unit, state) in &files {
            if !unit.contains("@.") {
                rows.entry(unit.clone()).or_insert_with(
                    || json!({ "unit": unit, "description": null, "load": "not-loaded", "active": "inactive", "sub": "dead", "enabled": state }),
                );
            }
        }
        let count = |f: &dyn Fn(&Value) -> bool| rows.values().filter(|r| f(r)).count();
        let running = count(&|r| r["sub"] == "running");
        let failed = count(&|r| r["active"] == "failed");

        let filter = a.filter.map(|f| f.to_lowercase());
        let mut matched: Vec<Value> = rows
            .into_values()
            .filter(|r| match a.state {
                StateFilter::All => true,
                StateFilter::Running => r["sub"] == "running",
                StateFilter::Failed => r["active"] == "failed",
                StateFilter::Enabled => r["enabled"] == "enabled",
            })
            .filter(|r| match &filter {
                Some(f) => [&r["unit"], &r["description"]]
                    .iter()
                    .any(|v| v.as_str().is_some_and(|s| s.to_lowercase().contains(f))),
                None => true,
            })
            .collect();
        let total = matched.len();
        matched.truncate(a.limit);
        Ok(json!({ "running": running, "failed": failed, "matched": total, "truncated": total > matched.len(), "units": matched }))
    }
}

pub struct SvcStatus;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    unit: String,
    #[serde(default)]
    scope: Scope,
    #[serde(default = "default_log_lines")]
    log_lines: usize,
}

fn default_log_lines() -> usize {
    20
}

const SHOW: &str = "Id,Description,LoadState,ActiveState,SubState,UnitFileState,MainPID,ActiveEnterTimestamp,\
                    InactiveEnterTimestamp,Result,NRestarts,MemoryCurrent,CPUUsageNSec,TasksCurrent,FragmentPath,User";

/// A unit's state, in readable units.
fn status(scope: Scope, unit: &str) -> Result<Value> {
    let p = parse_show(&systemctl(scope, &["show", "-p", SHOW, "--", unit])?);
    let get = |k: &str| p.get(k).filter(|v| !v.is_empty() && *v != "[not set]").cloned();
    let num = |k: &str| get(k).and_then(|v| v.parse::<u64>().ok()).filter(|n| *n != u64::MAX);
    if get("LoadState").as_deref() == Some("not-found") {
        bail!("there is no unit {unit}");
    }
    Ok(json!({
        "unit": get("Id").unwrap_or_else(|| unit.to_string()),
        "description": get("Description"),
        "load": get("LoadState"),
        "active": get("ActiveState"),
        "sub": get("SubState"),
        "enabled": get("UnitFileState"),
        "result": get("Result"),
        "main_pid": num("MainPID").filter(|n| *n != 0),
        "user": get("User"),
        "since": get("ActiveEnterTimestamp"),
        "stopped_at": get("InactiveEnterTimestamp"),
        "restarts": num("NRestarts"),
        "memory_mib": num("MemoryCurrent").map(|b| b / (1 << 20)),
        "cpu_secs": num("CPUUsageNSec").map(|n| n as f64 / 1e9),
        "tasks": num("TasksCurrent"),
        "unit_file": get("FragmentPath"),
    }))
}

/// The unit's most recent log lines, or why they can't be read.
fn journal(scope: Scope, unit: &str, lines: usize) -> Value {
    let n = lines.min(200).to_string();
    let mut args: Vec<&str> = scope.flag().into_iter().collect();
    args.extend(["-u", unit, "-n", &n, "--no-pager", "-q", "-o", "short-iso"]);
    match exec::output("journalctl", &args) {
        Ok(text) => json!(text.lines().collect::<Vec<_>>()),
        Err(e) => json!(format!("(the log can't be read: {e:#})")),
    }
}

impl Tool for SvcStatus {
    fn name(&self) -> &'static str {
        "svc_status"
    }

    fn description(&self) -> &'static str {
        "Show one systemd unit: its state, why it last stopped, its main process, memory, CPU time and \
         restarts, whether it starts at boot, and its most recent log lines."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "unit": { "type": "string", "description": "Unit name; \".service\" is added if it has no type." },
                "scope": schema_scope(),
                "log_lines": { "type": "integer", "minimum": 0, "maximum": 200, "default": 20 }
            },
            "required": ["unit"],
            "additionalProperties": false
        })
    }

    fn call(&self, _policy: &Policy, args: Value) -> Result<Value> {
        let a: StatusArgs = parse_args(args)?;
        let unit = unit_name(&a.unit)?;
        let mut st = status(a.scope, &unit)?;
        if a.log_lines > 0 {
            st["log"] = journal(a.scope, &unit, a.log_lines);
        }
        Ok(st)
    }
}

pub struct SvcControl;

#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Action {
    Start,
    Stop,
    Restart,
    Reload,
    Enable,
    Disable,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Start => "start",
            Action::Stop => "stop",
            Action::Restart => "restart",
            Action::Reload => "reload",
            Action::Enable => "enable",
            Action::Disable => "disable",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlArgs {
    unit: String,
    action: Action,
    #[serde(default)]
    scope: Scope,
}

fn check_control(policy: &Policy, a: &ControlArgs) -> Result<String> {
    check_scoped(policy, &a.unit, a.scope)
}

/// A unit Orion may change, for other tools (`gpu_assign`).
pub(super) fn check_unit(policy: &Policy, unit: &str, user: bool) -> Result<String> {
    check_scoped(policy, unit, if user { Scope::User } else { Scope::System })
}

fn check_scoped(policy: &Policy, unit: &str, scope: Scope) -> Result<String> {
    let unit = unit_name(unit)?;
    ensure!(
        CONTROLLABLE.iter().any(|s| unit.ends_with(s)),
        "only services, sockets, timers and paths can be controlled, not {unit}"
    );
    if let Some(p) = policy.services.protected.iter().find(|p| glob_match(p, &unit)) {
        bail!("{unit} is protected by the Orion policy (services.protected: {p:?}); no approval can change it");
    }
    systemd_running(scope)?;
    Ok(unit)
}

/// `services.allowed` pre-approves changes to a unit.
pub(super) fn preapproval(policy: &Policy, args: &Value) -> Preapproval {
    let unit = args.get("unit").and_then(Value::as_str).and_then(|u| unit_name(u).ok()).unwrap_or_default();
    if policy.services.allowed.iter().any(|p| glob_match(p, &unit)) {
        Preapproval::Granted
    } else {
        Preapproval::Refused(format!("{unit:?} is not in services.allowed"))
    }
}

/// Write (or with `None`, remove) a unit's drop-in file; as root for the
/// machine's units.
pub(super) fn write_drop_in(policy: &Policy, path: &Path, content: Option<&str>, user: bool) -> Result<()> {
    if user {
        match content {
            Some(c) => {
                std::fs::create_dir_all(path.parent().context("no directory")?)?;
                std::fs::write(path, c)?;
            }
            None => match std::fs::remove_file(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            },
        }
        return Ok(());
    }
    let prefix = exec::elevation(&policy.admin.elevate)?;
    let p = path.display().to_string();
    let dir = path.parent().context("no directory")?.display().to_string();
    let steps: Vec<(Vec<String>, Option<String>)> = match content {
        Some(c) => vec![
            (vec!["mkdir".into(), "-p".into(), "--".into(), dir], None),
            (vec!["tee".into(), "--".into(), p], Some(c.to_string())),
        ],
        None => vec![(vec!["rm".into(), "-f".into(), "--".into(), p], None)],
    };
    for (mut argv, stdin) in steps {
        let program = argv.remove(0);
        let mut cmd = exec::command_as(prefix.as_deref(), &program, &argv);
        cmd.stdout(std::process::Stdio::null());
        let done = exec::run(cmd, stdin, Duration::from_secs(30), 16 << 10)?;
        ensure!(done.success(), "{program}: {}", done.complaint());
    }
    Ok(())
}

/// After a drop-in changed: reload systemd, and restart the unit if it is
/// running. Whether it was restarted.
pub(super) fn reload_and_restart(policy: &Policy, unit: &str, user: bool) -> Result<bool> {
    let scope = if user { Scope::User } else { Scope::System };
    let prefix = if user { None } else { exec::elevation(&policy.admin.elevate)? };
    let run = |args: &[&str]| -> Result<exec::Finished> {
        let mut argv: Vec<String> = scope.flag().map(String::from).into_iter().collect();
        argv.push("--no-ask-password".into());
        argv.extend(args.iter().map(|a| a.to_string()));
        exec::run(exec::command_as(prefix.as_deref(), "systemctl", &argv), None, Duration::from_secs(120), 16 << 10)
    };
    let reload = run(&["daemon-reload"])?;
    ensure!(reload.success(), "systemctl daemon-reload: {}", reload.complaint());
    let active = status(scope, unit).is_ok_and(|s| s["active"] == "active");
    if active {
        let r = run(&["restart", "--", unit])?;
        ensure!(r.success(), "restarting {unit}: {}", r.complaint());
    }
    Ok(active)
}

impl Tool for SvcControl {
    fn name(&self) -> &'static str {
        "svc_control"
    }

    fn description(&self) -> &'static str {
        "Start, stop, restart or reload a systemd unit, or enable or disable it at boot (enabling does not \
         start it). Returns the unit's new state, with its log when the action failed. Units that keep the \
         machine reachable or running (ssh, networking, the display manager, systemd itself, Orion) are protected."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "unit": { "type": "string", "description": "Unit name; \".service\" is added if it has no type." },
                "action": { "type": "string", "enum": ["start", "stop", "restart", "reload", "enable", "disable"] },
                "scope": schema_scope()
            },
            "required": ["unit", "action"],
            "additionalProperties": false
        })
    }

    fn preapproval(&self, policy: &Policy, args: &Value) -> Preapproval {
        preapproval(policy, args)
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        check_control(policy, &parse_args(args.clone())?).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: ControlArgs = parse_args(args)?;
        let unit = check_control(policy, &a)?;
        let mut argv: Vec<String> = a.scope.flag().map(String::from).into_iter().collect();
        argv.extend(["--no-ask-password".into(), a.action.as_str().into(), "--".into(), unit.clone()]);
        let attempt = |prefix: Option<&[&str]>| {
            exec::run(exec::command_as(prefix, "systemctl", &argv), None, Duration::from_secs(120), 64 << 10)
        };
        let mut done = attempt(None)?;
        // System services as a normal user: polkit said no, so try root.
        let denied = |d: &exec::Finished| {
            let e = d.stderr.to_lowercase();
            e.contains("interactive authentication required") || e.contains("access denied")
        };
        let mut elevated = false;
        if !done.success() && a.scope == Scope::System && denied(&done) {
            if let Some(prefix) = exec::elevation(&policy.admin.elevate)? {
                done = attempt(Some(&prefix))?;
                elevated = true;
            }
        }
        let mut st = status(a.scope, &unit).unwrap_or_else(|e| json!({ "unit": unit, "state_error": format!("{e:#}") }));
        st["action"] = json!(a.action.as_str());
        st["ok"] = json!(done.success());
        st["elevated"] = json!(elevated);
        if !done.success() {
            st["error"] = json!(done.complaint());
            st["log"] = journal(a.scope, &unit, 15);
        }
        Ok(st)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_removes_user_drop_ins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ollama.service.d/50-orion-gpu.conf");
        let p = Policy::default();
        write_drop_in(&p, &path, Some("[Service]\nEnvironment=\"CUDA_VISIBLE_DEVICES=GPU-1\"\n"), true).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("CUDA_VISIBLE_DEVICES=GPU-1"));
        write_drop_in(&p, &path, None, true).unwrap();
        assert!(!path.exists());
        write_drop_in(&p, &path, None, true).unwrap();
    }

    #[test]
    fn unit_names() {
        assert_eq!(unit_name("ollama").unwrap(), "ollama.service");
        assert_eq!(unit_name("getty@tty1").unwrap(), "getty@tty1.service");
        assert_eq!(unit_name("backup.timer").unwrap(), "backup.timer");
        for bad in ["", "--now", "a b", "x;reboot", "../x", "a\nb"] {
            assert!(unit_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn parses_systemctl_output() {
        let units = parse_units(
            "cron.service loaded active running Regular background program processing daemon\n\
             ● nginx.service loaded failed failed A high performance web server\n",
        );
        assert_eq!(units[0].0, "cron.service");
        assert_eq!(units[0].1, ["loaded", "active", "running"].map(String::from));
        assert_eq!(units[0].2, "Regular background program processing daemon");
        assert_eq!(units[1].0, "nginx.service");
        assert_eq!(units[1].1[1], "failed");

        let files = parse_unit_files("cron.service enabled enabled\nollama.service disabled enabled\n");
        assert_eq!(files["ollama.service"], "disabled");

        let show = parse_show("Id=cron.service\nMainPID=812\nMemoryCurrent=[not set]\nDescription=a=b\n");
        assert_eq!(show["Description"], "a=b");
    }

    #[test]
    fn control_is_scoped() {
        let p = Policy::from_toml("[services]\nallowed = [\"ollama*\"]\n").unwrap();
        let args = |unit: &str| json!({ "unit": unit, "action": "restart" });
        assert!(matches!(SvcControl.preapproval(&p, &args("ollama")), Preapproval::Granted));
        assert!(matches!(SvcControl.preapproval(&p, &args("nginx")), Preapproval::Refused(_)));

        for unit in ["ssh", "sshd.service", "systemd-logind", "orion.socket", "reboot.target", "getty@tty1", "home.mount"] {
            let err = SvcControl.precheck(&p, &args(unit)).unwrap_err();
            assert!(format!("{err:#}").contains("protected") || format!("{err:#}").contains("only services"), "{unit}: {err:#}");
        }
        assert!(SvcControl.precheck(&p, &json!({ "unit": "ollama", "action": "reboot" })).is_err());
    }
}
