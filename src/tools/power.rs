//! Power: batteries and the charger (from /sys/class/power_supply), the
//! power profile (power-profiles-daemon, or tuned's compatible service),
//! and locking, suspending, hibernating, rebooting and powering off,
//! through logind as the person at the machine would.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, ensure, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{exec, session};
use crate::policy::Policy;
use crate::tool::{parse_args, Preapproval, Risk, Tool};

fn read(dir: &Path, file: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(file)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn number(dir: &Path, file: &str) -> Option<f64> {
    read(dir, file)?.parse().ok()
}

/// Every battery and charger under `root` (/sys/class/power_supply).
fn supplies(root: &Path) -> (Vec<Value>, Option<bool>) {
    let Ok(entries) = std::fs::read_dir(root) else { return (Vec::new(), None) };
    let mut dirs: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    dirs.sort();
    let mut batteries = Vec::new();
    let mut on_ac: Option<bool> = None;
    for dir in dirs {
        match read(&dir, "type").as_deref() {
            Some("Mains") | Some("USB") => {
                let online = read(&dir, "online").as_deref() == Some("1");
                on_ac = Some(on_ac.unwrap_or(false) || online);
            }
            // Peripherals (a mouse's battery) say scope Device.
            Some("Battery") if read(&dir, "scope").as_deref() != Some("Device") => batteries.push(battery(&dir)),
            _ => {}
        }
    }
    (batteries, on_ac)
}

fn battery(dir: &Path) -> Value {
    // Energy (µWh) or charge (µAh): drivers give one or the other.
    let (now, full, design, rate) = match number(dir, "energy_now") {
        Some(now) => (Some(now), number(dir, "energy_full"), number(dir, "energy_full_design"), number(dir, "power_now")),
        None => (number(dir, "charge_now"), number(dir, "charge_full"), number(dir, "charge_full_design"), number(dir, "current_now")),
    };
    let status = read(dir, "status").unwrap_or_else(|| "Unknown".into());
    let capacity = number(dir, "capacity").or(now.zip(full).filter(|(_, f)| *f > 0.0).map(|(n, f)| (n / f * 100.0).round()));
    let rate = rate.filter(|r| *r > 0.0);
    let hours = match status.as_str() {
        "Discharging" => now.zip(rate).map(|(n, r)| n / r),
        "Charging" => now.zip(full).zip(rate).map(|((n, f), r)| (f - n).max(0.0) / r),
        _ => None,
    };
    json!({
        "name": dir.file_name().map(|n| n.to_string_lossy().into_owned()),
        "percent": capacity,
        "status": status.to_lowercase(),
        "minutes_left": hours.map(|h| (h * 60.0).round() as u64),
        // How much of what it held when new it still holds.
        "health_percent": full.zip(design).filter(|(_, d)| *d > 0.0).map(|(f, d)| (f / d * 100.0).round()),
        "cycles": number(dir, "cycle_count").filter(|c| *c > 0.0),
        "model": read(dir, "model_name"),
        "manufacturer": read(dir, "manufacturer"),
    })
}

/// `powerprofilesctl list`: the profiles, the current one starred.
fn parse_profiles(text: &str) -> (Vec<String>, Option<String>) {
    let mut all = Vec::new();
    let mut current = None;
    for line in text.lines() {
        let starred = line.starts_with('*');
        let name = line.trim_start_matches(['*', ' ']);
        if line.starts_with("    ") || !name.ends_with(':') || name.contains(' ') {
            continue;
        }
        let name = name.trim_end_matches(':').to_string();
        if starred {
            current = Some(name.clone());
        }
        all.push(name);
    }
    (all, current)
}

fn profiles() -> Option<(Vec<String>, Option<String>)> {
    exec::installed("powerprofilesctl").then(|| exec::output("powerprofilesctl", &["list"]).ok()).flatten().map(|t| parse_profiles(&t))
}

/// logind's answer to Can`what` ("yes", "no", "challenge" or "na").
fn can(what: &str) -> Option<String> {
    let text = exec::output("busctl", &["--system", "call", "org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager", &format!("Can{what}")]).ok()?;
    // `s "yes"`
    text.trim().strip_prefix("s ").map(|s| s.trim_matches('"').to_string())
}

pub struct PowerStatus;

impl Tool for PowerStatus {
    fn name(&self) -> &'static str {
        "power_status"
    }

    fn description(&self) -> &'static str {
        "Power: each battery's charge, whether it is charging, time left, and its health (how much of its \
         original capacity it keeps); whether the charger is in; the power profile (performance, balanced, \
         power-saver); and whether the machine can suspend or hibernate."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn call(&self, _policy: &Policy, _args: Value) -> Result<Value> {
        let (batteries, on_ac) = supplies(Path::new("/sys/class/power_supply"));
        let (available, profile) = profiles().unwrap_or_default();
        Ok(json!({
            "batteries": batteries,
            "on_ac": on_ac.or(batteries.is_empty().then_some(true)),
            "profile": profile,
            "profiles": available,
            "can_suspend": can("Suspend"),
            "can_hibernate": can("Hibernate"),
        }))
    }
}

pub struct PowerSet;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileArgs {
    profile: String,
}

fn check_profile(a: &ProfileArgs) -> Result<()> {
    let (available, _) = profiles().ok_or_else(|| anyhow::anyhow!("power profiles need power-profiles-daemon (or tuned-ppd) and powerprofilesctl"))?;
    ensure!(available.contains(&a.profile), "{:?} isn't a power profile here; there are: {}", a.profile, available.join(", "));
    Ok(())
}

impl Tool for PowerSet {
    fn name(&self) -> &'static str {
        "power_set"
    }

    fn description(&self) -> &'static str {
        "Switch the power profile: performance (fastest, loudest), balanced, or power-saver (longest battery)."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "profile": { "type": "string", "enum": ["performance", "balanced", "power-saver"] } },
            "required": ["profile"],
            "additionalProperties": false
        })
    }

    fn preapproval(&self, policy: &Policy, _args: &Value) -> Preapproval {
        policy.desktop.preapproval("power_profile")
    }

    fn precheck(&self, _policy: &Policy, args: &Value) -> Result<()> {
        check_profile(&parse_args(args.clone())?)
    }

    fn call(&self, _policy: &Policy, args: Value) -> Result<Value> {
        let a: ProfileArgs = parse_args(args)?;
        check_profile(&a)?;
        exec::output("powerprofilesctl", &["set", &a.profile])?;
        let (_, now) = profiles().unwrap_or_default();
        Ok(json!({ "ok": now.as_deref() == Some(a.profile.as_str()), "profile": now }))
    }
}

pub struct PowerAction;

#[derive(Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
enum Action {
    Lock,
    Suspend,
    Hibernate,
    Reboot,
    Poweroff,
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Action::Lock => "lock",
            Action::Suspend => "suspend",
            Action::Hibernate => "hibernate",
            Action::Reboot => "reboot",
            Action::Poweroff => "poweroff",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionArgs {
    action: Action,
}

impl Tool for PowerAction {
    fn name(&self) -> &'static str {
        "power_action"
    }

    fn description(&self) -> &'static str {
        "Lock the screen, suspend, hibernate, reboot or power off the machine, as the person at it would \
         (logind decides, and may refuse while others are logged in). Make sure nothing unsaved is lost \
         before rebooting or powering off."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "action": { "type": "string", "enum": ["lock", "suspend", "hibernate", "reboot", "poweroff"] } },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    fn preapproval(&self, policy: &Policy, args: &Value) -> Preapproval {
        match parse_args::<ActionArgs>(args.clone()) {
            Ok(a) => policy.desktop.preapproval(a.action.name()),
            Err(e) => Preapproval::Refused(format!("{e:#}")),
        }
    }

    fn call(&self, _policy: &Policy, args: Value) -> Result<Value> {
        let a: ActionArgs = parse_args(args)?;
        let done = match a.action {
            Action::Lock => {
                // The graphical session's id: a user service belongs to no session.
                let id = session::login_session();
                let mut cmd = std::process::Command::new("loginctl");
                cmd.arg("lock-session").args(id);
                exec::run(cmd, None, Duration::from_secs(10), 16 << 10)?
            }
            other => {
                let mut cmd = std::process::Command::new("systemctl");
                cmd.args(["--no-ask-password", other.name()]);
                exec::run(cmd, None, Duration::from_secs(30), 16 << 10)?
            }
        };
        if !done.success() {
            let why = done.complaint();
            if why.contains("interactive authentication") || why.contains("Access denied") {
                bail!("logind won't {} without a password here (other people may be logged in): {why}", a.action.name());
            }
            bail!("{}: {why}", a.action.name());
        }
        Ok(json!({ "ok": true, "action": a.action.name() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_batteries_and_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let put = |name: &str, files: &[(&str, &str)]| {
            let d = dir.path().join(name);
            std::fs::create_dir(&d).unwrap();
            for (f, v) in files {
                std::fs::write(d.join(f), format!("{v}\n")).unwrap();
            }
        };
        put("AC", &[("type", "Mains"), ("online", "0")]);
        put("BAT0", &[("type", "Battery"), ("status", "Discharging"), ("capacity", "62"), ("energy_now", "31000000"),
                      ("energy_full", "50000000"), ("energy_full_design", "57000000"), ("power_now", "9300000"), ("cycle_count", "212")]);
        put("hidpp_battery_0", &[("type", "Battery"), ("scope", "Device"), ("capacity", "90")]);
        let (b, ac) = supplies(dir.path());
        assert_eq!((b.len(), ac), (1, Some(false)));
        assert_eq!((b[0]["percent"].as_f64(), b[0]["minutes_left"].as_u64(), b[0]["health_percent"].as_f64()), (Some(62.0), Some(200), Some(88.0)));
        assert_eq!(b[0]["cycles"].as_f64(), Some(212.0));
        assert_eq!(supplies(&dir.path().join("none")), (Vec::new(), None));

        let (all, current) = parse_profiles("  performance:\n    CpuDriver:\tintel_pstate\n    Degraded:   no\n\n* balanced:\n    CpuDriver:\tintel_pstate\n\n  power-saver:\n    CpuDriver:\tintel_pstate\n");
        assert_eq!((all, current.as_deref()), (vec!["performance".to_string(), "balanced".into(), "power-saver".into()], Some("balanced")));

        // Rebooting is never pre-approved by default; locking is.
        let p = Policy::from_toml("").unwrap();
        assert!(matches!(PowerAction.preapproval(&p, &json!({ "action": "lock" })), Preapproval::Granted));
        assert!(matches!(PowerAction.preapproval(&p, &json!({ "action": "reboot" })), Preapproval::NotApplicable));
    }
}
