//! Timers: systemd timers on this machine, and scheduled Orion tool calls.
//!
//! A timer Orion makes runs one tool call, `orion call <tool> <args>`, as a
//! systemd user timer. So when it fires, the call goes through the policy,
//! the sandbox and the audit log like any other, with nobody there to ask:
//! only calls the policy allows or pre-approves run. `timer_create` refuses
//! a schedule that would only be refused when it fires.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::exec;
use crate::policy::{Decision, Policy};
use crate::tool::{parse_args, Preapproval, Risk, Tool};

const PREFIX: &str = "orion-timer-";

fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    ensure!(ok, "{name:?} isn't a timer name: lowercase letters, digits and -, up to 40");
    Ok(())
}

/// A schedule systemd takes (`OnCalendar=`): "daily", "Mon..Fri 08:30",
/// "*-*-* 06:00". Checked, and its next runs, by systemd-analyze.
fn check_schedule(schedule: &str) -> Result<(String, Vec<String>)> {
    let ok = !schedule.is_empty()
        && schedule.len() <= 100
        && schedule.chars().all(|c| c.is_ascii_alphanumeric() || " :*,./~-".contains(c));
    ensure!(ok, "{schedule:?} isn't a schedule (like \"daily\", \"Mon..Fri 08:30\" or \"*-*-* 06:00\")");
    if !exec::installed("systemd-analyze") {
        return Ok((schedule.to_string(), Vec::new()));
    }
    let args = ["calendar".to_string(), "--iterations=3".into(), "--".into(), schedule.to_string()];
    let done = exec::run(exec::command_as(None, "systemd-analyze", &args), None, Duration::from_secs(10), 16 << 10)?;
    ensure!(done.success(), "systemd doesn't take {schedule:?} as a schedule: {}", done.complaint());
    Ok(parse_calendar(&done.stdout))
}

/// `systemd-analyze calendar` output: the normalized form, and the next runs.
fn parse_calendar(text: &str) -> (String, Vec<String>) {
    let mut normalized = String::new();
    let mut next = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(": ") else { continue };
        match key.trim() {
            "Normalized form" => normalized = value.trim().to_string(),
            k if k == "Next elapse" || k.starts_with("Iteration #") => next.push(value.trim().to_string()),
            _ => {}
        }
    }
    (normalized, next)
}

/// One argument on an `ExecStart=` line: quoted, with systemd's own
/// specials (`%`, `$`) escaped, so it reaches the program as is.
fn systemd_quote(arg: &str) -> String {
    let mut out = String::from('"');
    for c in arg.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn unit_dir() -> Result<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("no home folder for systemd user units")?;
    Ok(config.join("systemd/user"))
}

/// The service and timer units for a scheduled call.
fn units(orion: &str, name: &str, schedule: &str, tool: &str, args: &Value, config: Option<&str>) -> (String, String) {
    let args = args.to_string();
    let env = config.map(|c| format!("Environment={}\n", systemd_quote(&format!("ORION_CONFIG={c}")))).unwrap_or_default();
    let service = format!(
        "# Made by Orion (timer_create). It runs one tool call through Orion's policy,\n\
         # sandbox and audit log; remove it with timer_remove.\n\
         [Unit]\n\
         Description=Orion timer {name}: {tool}\n\
         X-OrionTool={tool}\n\
         X-OrionArgs={}\n\n\
         [Service]\n\
         Type=oneshot\n\
         {env}\
         ExecStart={} call --caller {} {} {}\n",
        args.replace('\n', " "),
        systemd_quote(orion),
        systemd_quote(&format!("timer {name}")),
        systemd_quote(tool),
        systemd_quote(&args)
    );
    let timer = format!(
        "# Made by Orion (timer_create).\n\
         [Unit]\n\
         Description=Orion timer {name}\n\n\
         [Timer]\n\
         OnCalendar={schedule}\n\
         Persistent=true\n\n\
         [Install]\n\
         WantedBy=timers.target\n"
    );
    (service, timer)
}

/// Whether `tool` with `args` would run with nobody there to approve it.
fn unattended(policy: &Policy, tool: &dyn Tool, args: &Value) -> Result<()> {
    let name = tool.name();
    match (policy.decide(name, tool.risk()), tool.preapproval(policy, args)) {
        (Decision::Deny, _) => bail!("{name} is denied by the policy"),
        (Decision::Allow, Preapproval::Refused(why)) => bail!("{name} would be refused when it runs: {why}"),
        (Decision::Allow, _) | (Decision::Ask, Preapproval::Granted) => Ok(()),
        (Decision::Ask, _) => bail!(
            "{name} needs approval, and nobody is there to give it when a timer fires. Pre-approve the call in the \
             policy first (for proc_run, [proc] allowed_commands; for update_apply, [packages] updates; or [tools] \
             {name} = \"allow\")"
        ),
    }
}

fn systemctl_user(args: &[&str]) -> Result<exec::Finished> {
    let mut argv = vec!["--user".to_string()];
    argv.extend(args.iter().map(|a| a.to_string()));
    exec::run(exec::command_as(None, "systemctl", &argv), None, Duration::from_secs(30), 64 << 10)
}

pub struct TimerList;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

/// `systemctl show` output for several units: one map per blank-line block.
fn parse_show(text: &str) -> Vec<std::collections::BTreeMap<String, String>> {
    text.split("\n\n")
        .map(|block| block.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect())
        .filter(|m: &std::collections::BTreeMap<String, String>| m.contains_key("Id"))
        .collect()
}

/// `{ OnCalendar=*-*-* 06:00:00 ; next_elapse=… }` → `*-*-* 06:00:00`.
fn calendar_of(timers_calendar: &str) -> Option<String> {
    let rest = timers_calendar.split_once("OnCalendar=")?.1;
    Some(rest.split(" ;").next()?.trim().to_string())
}

fn list_scope(user: bool) -> Result<Vec<Value>> {
    let scope = if user { vec!["--user"] } else { vec![] };
    let mut args = scope.clone();
    args.extend(["list-units", "--type=timer", "--all", "--plain", "--no-legend", "--no-pager"]);
    let argv: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let done = exec::run(exec::command_as(None, "systemctl", &argv), None, Duration::from_secs(30), 1 << 20)?;
    ensure!(done.success(), "systemctl: {}", done.complaint());
    let names: Vec<String> = done.stdout.lines().filter_map(|l| l.split_whitespace().next()).map(String::from).collect();
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let mut show: Vec<String> = scope.iter().map(|s| s.to_string()).collect();
    show.extend(["show".into(), "-p".into(), "Id,Description,ActiveState,NextElapseUSecRealtime,LastTriggerUSec,Unit,TimersCalendar".into(), "--".into()]);
    show.extend(names);
    let done = exec::run(exec::command_as(None, "systemctl", &show), None, Duration::from_secs(30), 4 << 20)?;
    let dir = unit_dir().ok();
    Ok(parse_show(&done.stdout)
        .into_iter()
        .map(|m| {
            let id = m.get("Id").cloned().unwrap_or_default();
            let ours = user && id.starts_with(PREFIX);
            let mut v = json!({
                "timer": id,
                "scope": if user { "user" } else { "system" },
                "description": m.get("Description"),
                "active": m.get("ActiveState").map(|s| s == "active"),
                "schedule": m.get("TimersCalendar").and_then(|c| calendar_of(c)),
                "next": m.get("NextElapseUSecRealtime").filter(|s| !s.is_empty()),
                "last": m.get("LastTriggerUSec").filter(|s| !s.is_empty() && *s != "n/a"),
                "runs": m.get("Unit"),
                "orion": ours,
            });
            // Orion's own: what the call is, from the service unit.
            if let (true, Some(dir)) = (ours, &dir) {
                let service = id.trim_end_matches(".timer").to_string() + ".service";
                if let Ok(text) = std::fs::read_to_string(dir.join(service)) {
                    let field = |k: &str| text.lines().find_map(|l| l.strip_prefix(k)).map(str::to_string);
                    v["tool"] = json!(field("X-OrionTool="));
                    v["args"] = field("X-OrionArgs=").and_then(|a| serde_json::from_str::<Value>(&a).ok()).unwrap_or(Value::Null);
                }
            }
            v
        })
        .collect())
}

impl Tool for TimerList {
    fn name(&self) -> &'static str {
        "timer_list"
    }

    fn description(&self) -> &'static str {
        "List the systemd timers on this machine (the system's and your own): what each runs, its schedule, and when \
         it last ran and runs next. Orion's own scheduled calls are marked, with the tool call each makes."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn call(&self, _policy: &Policy, args: Value) -> Result<Value> {
        let NoArgs {} = parse_args(args)?;
        ensure!(std::path::Path::new("/run/systemd/system").exists(), "systemd is not running on this machine (or Orion runs in a container without it)");
        let mut timers = list_scope(false)?;
        let user = list_scope(true);
        let mut out = json!({});
        match user {
            Ok(t) => timers.extend(t),
            Err(e) => out["note"] = json!(format!("your own timers couldn't be listed: {e:#}")),
        }
        out["count"] = json!(timers.len());
        out["timers"] = json!(timers);
        Ok(out)
    }
}

pub struct TimerCreate;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateArgs {
    name: String,
    schedule: String,
    tool: String,
    #[serde(default = "empty_object")]
    args: Value,
}

fn empty_object() -> Value {
    json!({})
}

/// Everything about a timer that can be checked before anyone is asked.
fn check_create(policy: &Policy, a: &CreateArgs) -> Result<(String, Vec<String>)> {
    check_name(&a.name)?;
    ensure!(std::path::Path::new("/run/systemd/system").exists(), "systemd is not running on this machine (or Orion runs in a container without it)");
    let schedule = check_schedule(&a.schedule)?;
    ensure!(a.args.is_object(), "args is a JSON object, the tool's arguments");
    ensure!(a.args.to_string().len() <= 8192, "the arguments are too long for a timer");
    ensure!(!a.tool.starts_with("timer_"), "a timer can't make or remove timers");
    let tools = super::builtin();
    let tool = tools.iter().find(|t| t.name() == a.tool).with_context(|| format!("there is no tool {:?}", a.tool))?;
    unattended(policy, tool.as_ref(), &a.args)?;
    tool.precheck(policy, &a.args).with_context(|| format!("{} would fail as asked", a.tool))?;
    Ok(schedule)
}

impl Tool for TimerCreate {
    fn name(&self) -> &'static str {
        "timer_create"
    }

    fn description(&self) -> &'static str {
        "Schedule a tool call to run on its own: daily, weekly, \"Mon..Fri 08:30\", or any systemd calendar \
         schedule, as a timer of yours. When it fires, the call goes through the policy, sandbox and audit log as \
         usual, with nobody there to approve it, so only calls the policy allows or pre-approves can be scheduled. \
         Replaces a timer of the same name."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Lowercase letters, digits and -, such as \"nightly-updates\"." },
                "schedule": { "type": "string", "description": "\"daily\", \"weekly\", \"Mon..Fri 08:30\", \"*-*-* 06:00\"." },
                "tool": { "type": "string", "description": "The tool to call, such as \"update_apply\"." },
                "args": { "type": "object", "description": "Its arguments, as for calling it now." }
            },
            "required": ["name", "schedule", "tool"],
            "additionalProperties": false
        })
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        check_create(policy, &parse_args(args.clone())?).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: CreateArgs = parse_args(args)?;
        let (normalized, next) = check_create(policy, &a)?;
        let orion = std::env::current_exe().context("finding Orion's own program")?;
        let orion = orion.to_str().context("Orion's path isn't text")?.trim_end_matches(" (deleted)");
        let config = std::env::var("ORION_CONFIG").ok();
        let (service, timer) = units(orion, &a.name, &a.schedule, &a.tool, &a.args, config.as_deref());
        let dir = unit_dir()?;
        std::fs::create_dir_all(&dir)?;
        let unit = format!("{PREFIX}{}", a.name);
        let files = [dir.join(format!("{unit}.service")), dir.join(format!("{unit}.timer"))];
        std::fs::write(&files[0], service)?;
        std::fs::write(&files[1], timer)?;
        let started = systemctl_user(&["daemon-reload"]).and_then(|r| {
            ensure!(r.success(), "systemd's user instance didn't take the timer: {}", r.complaint());
            let e = systemctl_user(&["enable", "--now", &format!("{unit}.timer")])?;
            ensure!(e.success(), "starting the timer: {}", e.complaint());
            Ok(())
        });
        if let Err(e) = started {
            // Leave nothing half-made behind.
            for f in &files {
                let _ = std::fs::remove_file(f);
            }
            return Err(e);
        }
        let mut out = json!({ "timer": format!("{unit}.timer"), "schedule": normalized, "next_runs": next, "tool": a.tool, "args": a.args });
        // A user timer runs only while you're logged in, unless lingering is on.
        let user = std::env::var("USER").unwrap_or_default();
        let linger = exec::output("loginctl", &["show-user", &user, "-p", "Linger", "--value"]).unwrap_or_default();
        if linger.trim() != "yes" {
            out["note"] = json!("it runs only while you are logged in; `sudo loginctl enable-linger $USER` makes it run regardless");
        }
        Ok(out)
    }
}

pub struct TimerRemove;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveArgs {
    name: String,
}

impl Tool for TimerRemove {
    fn name(&self) -> &'static str {
        "timer_remove"
    }

    fn description(&self) -> &'static str {
        "Stop and remove a timer Orion made with timer_create, by its name. Other timers are the system's or yours \
         to manage with svc_control."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "name": { "type": "string", "description": "As given to timer_create (or with orion-timer- in front)." } },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    fn precheck(&self, _policy: &Policy, args: &Value) -> Result<()> {
        let a: RemoveArgs = parse_args(args.clone())?;
        let name = a.name.trim_start_matches(PREFIX).trim_end_matches(".timer");
        check_name(name)?;
        let file = unit_dir()?.join(format!("{PREFIX}{name}.timer"));
        ensure!(file.exists(), "Orion has no timer called {name} (timer_list shows them)");
        Ok(())
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        self.precheck(policy, &args)?;
        let a: RemoveArgs = parse_args(args)?;
        let name = a.name.trim_start_matches(PREFIX).trim_end_matches(".timer");
        let unit = format!("{PREFIX}{name}");
        let stop = systemctl_user(&["disable", "--now", &format!("{unit}.timer")])?;
        let dir = unit_dir()?;
        for ext in ["timer", "service"] {
            let _ = std::fs::remove_file(dir.join(format!("{unit}.{ext}")));
        }
        let _ = systemctl_user(&["daemon-reload"]);
        Ok(json!({ "removed": format!("{unit}.timer"), "was_running": stop.success() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_schedules() {
        for ok in ["nightly-updates", "a", "backup2"] {
            assert!(check_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-x", "Nightly", "a b", "../x", "x.timer", &"a".repeat(41)] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
        for bad in ["daily\nExecStart=/bin/sh", "daily; rm -rf /", "$(id)", ""] {
            assert!(check_schedule(bad).is_err(), "{bad:?}");
        }
        if exec::installed("systemd-analyze") {
            let (normalized, next) = check_schedule("Mon..Fri 08:30").unwrap();
            assert_eq!(normalized, "Mon..Fri *-*-* 08:30:00");
            assert_eq!(next.len(), 3);
            assert!(check_schedule("every tuesday").unwrap_err().to_string().contains("doesn't take"));
        }
        let (n, next) = parse_calendar("  Original form: daily\nNormalized form: *-*-* 00:00:00\n    Next elapse: Tue 2026-10-21 00:00:00 UTC\n       From now: 1h\n   Iteration #2: Wed 2026-10-22 00:00:00 UTC\n");
        assert_eq!(n, "*-*-* 00:00:00");
        assert_eq!(next, ["Tue 2026-10-21 00:00:00 UTC", "Wed 2026-10-22 00:00:00 UTC"]);
    }

    #[test]
    fn writes_units_systemd_reads_as_is() {
        assert_eq!(systemd_quote(r#"{"a":"50% $HOME \"x\" \\"}"#), r#""{\"a\":\"50%% $$HOME \\\"x\\\" \\\\\"}""#);
        let (service, timer) = units(
            "/usr/local/bin/orion",
            "nightly",
            "*-*-* 03:00",
            "update_apply",
            &json!({ "security_only": true }),
            Some("/etc/orion/me.toml"),
        );
        assert!(service.contains("ExecStart=\"/usr/local/bin/orion\" call --caller \"timer nightly\" \"update_apply\" \"{\\\"security_only\\\":true}\"\n"), "{service}");
        assert!(service.contains("Environment=\"ORION_CONFIG=/etc/orion/me.toml\""), "{service}");
        assert!(service.contains("X-OrionTool=update_apply\nX-OrionArgs={\"security_only\":true}\n"), "{service}");
        assert!(timer.contains("OnCalendar=*-*-* 03:00\nPersistent=true\n"), "{timer}");
        // systemd itself agrees the units are sound, where it can say.
        if exec::installed("systemd-analyze") {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("orion-timer-nightly.service"), &service).unwrap();
            std::fs::write(dir.path().join("orion-timer-nightly.timer"), &timer).unwrap();
            let out = std::process::Command::new("systemd-analyze")
                .args(["verify", "--man=no"])
                .arg(dir.path().join("orion-timer-nightly.timer"))
                .output()
                .unwrap();
            let complaint = String::from_utf8_lossy(&out.stderr);
            assert!(!complaint.contains("orion-timer-nightly"), "{complaint}");
        }

        let blocks = parse_show(
            "Id=apt-daily.timer\nDescription=Daily apt download activities\nActiveState=active\nNextElapseUSecRealtime=Tue 2026-10-20 18:01:12 UTC\n\
             LastTriggerUSec=n/a\nUnit=apt-daily.service\nTimersCalendar={ OnCalendar=*-*-* 06,18:00:00 ; next_elapse=Tue 2026-10-20 18:00:00 UTC }\n\n\
             Id=fstrim.timer\nUnit=fstrim.service\n",
        );
        assert_eq!(blocks.len(), 2);
        assert_eq!(calendar_of(&blocks[0]["TimersCalendar"]).as_deref(), Some("*-*-* 06,18:00:00"));
    }

    #[test]
    fn only_calls_that_would_run_unattended() {
        let p = Policy::from_toml("[proc]\nallowed_commands = [\"df\"]\n[packages]\nupdates = \"security\"\n").unwrap();
        let tools = super::super::builtin();
        let tool = |n: &str| tools.iter().find(|t| t.name() == n).unwrap().as_ref();
        assert!(unattended(&p, tool("system_info"), &json!({})).is_ok(), "read tools are allowed");
        assert!(unattended(&p, tool("proc_run"), &json!({ "command": "df" })).is_ok(), "pre-approved command");
        assert!(unattended(&p, tool("proc_run"), &json!({ "command": "rm" })).is_err());
        assert!(unattended(&p, tool("update_apply"), &json!({ "security_only": true })).is_ok());
        assert!(unattended(&p, tool("update_apply"), &json!({})).is_err());
        assert!(unattended(&p, tool("fs_delete"), &json!({ "path": "~/x" })).unwrap_err().to_string().contains("nobody is there"));
        let create = |tool: &str| check_create(&p, &CreateArgs { name: "x".into(), schedule: "daily".into(), tool: tool.into(), args: json!({}) });
        assert!(create("timer_create").is_err(), "no timers making timers");
        assert!(create("no_such_tool").is_err());
    }
}
