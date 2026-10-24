//! The clock and the language: time zone, network time, the system locale
//! and the keyboard layout, through systemd's timedated and localed
//! (`timedatectl`, `localectl`). Without systemd, the status comes from the
//! files in /etc.
//!
//! Changes are tried as the person first (polkit may let them through),
//! then as root through the privileged helper or sudo. Every value is
//! checked against what the machine has (/usr/share/zoneinfo, the locales
//! generated, the XKB layouts) before anything runs, and the helper checks
//! again.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::exec;
use crate::policy::Policy;
use crate::tool::{parse_args, Preapproval, Risk, Tool};

const ZONEINFO: &str = "/usr/share/zoneinfo";
const XKB_RULES: &str = "/usr/share/X11/xkb/rules/base.lst";

/// `timedatectl show`: `Key=Value` lines.
fn parse_keys(text: &str) -> BTreeMap<String, String> {
    text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect()
}

/// `localectl status`: `   System Locale: LANG=en_US.UTF-8` and so on.
fn parse_status(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut last = String::new();
    for line in text.lines() {
        match line.split_once(": ") {
            Some((k, v)) if !k.trim().is_empty() => {
                last = k.trim().to_string();
                out.insert(last.clone(), v.trim().to_string());
            }
            // Continuation lines (more locale variables).
            _ if !last.is_empty() && !line.trim().is_empty() => {
                if let Some(v) = out.get_mut(&last) {
                    v.push(' ');
                    v.push_str(line.trim());
                }
            }
            _ => {}
        }
    }
    out
}

/// The zone /etc/localtime points into.
fn zone_from_link(link: &Path) -> Option<String> {
    let target = std::fs::read_link(link).ok()?;
    let s = target.to_string_lossy();
    s.split_once("zoneinfo/").map(|(_, z)| z.to_string())
}

/// `LANG=…` from /etc/locale.conf or /etc/default/locale.
fn lang_from(files: &[&Path]) -> Option<String> {
    files.iter().find_map(|f| {
        std::fs::read_to_string(f).ok()?.lines().find_map(|l| l.trim().strip_prefix("LANG=").map(|v| v.trim_matches('"').to_string()))
    })
}

fn valid_zone(root: &Path, zone: &str) -> Result<()> {
    let ok = !zone.is_empty()
        && zone.len() <= 64
        && !zone.starts_with('/')
        && !zone.split('/').any(|p| p.is_empty() || p == "." || p == "..")
        && zone.chars().all(|c| c.is_ascii_alphanumeric() || "/_+-".contains(c));
    ensure!(ok, "{zone:?} isn't a time zone name (like Europe/Berlin or America/New_York)");
    let meta = std::fs::metadata(root.join(zone)).ok();
    ensure!(meta.is_some_and(|m| m.is_file()), "this machine has no time zone {zone:?} (timedatectl list-timezones lists them)");
    Ok(())
}

/// `en_US.UTF-8` and `en_US.utf8` are the same locale.
fn same_locale(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.to_lowercase().replace("utf-8", "utf8");
    norm(a) == norm(b)
}

fn valid_locale(locale: &str, available: &[String]) -> Result<()> {
    let ok = !locale.is_empty() && locale.len() <= 64 && locale.chars().all(|c| c.is_ascii_alphanumeric() || "_.@-".contains(c));
    ensure!(ok, "{locale:?} isn't a locale name (like de_DE.UTF-8)");
    ensure!(
        available.iter().any(|a| same_locale(a, locale)),
        "the locale {locale:?} isn't generated on this machine (add it with locale-gen, or your distribution's language packs)"
    );
    Ok(())
}

fn locales() -> Vec<String> {
    let listed = exec::output("localectl", &["list-locales", "--no-pager"]).or_else(|_| exec::output("locale", &["-a"]));
    listed.map(|t| t.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()).unwrap_or_default()
}

/// XKB layouts and variants, from base.lst: `! layout` lines `  de  German`,
/// `! variant` lines `  nodeadkeys  de: German (no dead keys)`.
fn parse_xkb(text: &str) -> (Vec<String>, Vec<(String, String)>) {
    let (mut layouts, mut variants) = (Vec::new(), Vec::new());
    let mut section = "";
    for line in text.lines() {
        if let Some(s) = line.strip_prefix("! ") {
            section = if s.trim() == "layout" { "layout" } else if s.trim() == "variant" { "variant" } else { "" };
            continue;
        }
        let mut f = line.split_whitespace();
        match (section, f.next(), f.next()) {
            ("layout", Some(name), _) => layouts.push(name.to_string()),
            ("variant", Some(name), Some(layout)) => variants.push((layout.trim_end_matches(':').to_string(), name.to_string())),
            _ => {}
        }
    }
    (layouts, variants)
}

fn valid_keyboard(rules: &str, layout: &str, variant: Option<&str>) -> Result<()> {
    let word = |s: &str| !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_alphanumeric() || "_-".contains(c));
    ensure!(word(layout), "{layout:?} isn't a keyboard layout (like us, de or fr)");
    ensure!(variant.is_none_or(word), "that isn't a keyboard variant");
    let (layouts, variants) = parse_xkb(rules);
    if layouts.is_empty() {
        return Ok(()); // no XKB data here to check against; localectl checks too
    }
    ensure!(layouts.iter().any(|l| l == layout), "there is no keyboard layout {layout:?} (localectl list-x11-keymap-layouts lists them)");
    if let Some(v) = variant {
        ensure!(variants.iter().any(|(l, n)| l == layout && n == v), "the {layout} layout has no variant {v:?}");
    }
    Ok(())
}

pub struct ClockStatus;

impl Tool for ClockStatus {
    fn name(&self) -> &'static str {
        "clock_status"
    }

    fn description(&self) -> &'static str {
        "The clock and language settings: local time, time zone, whether network time (NTP) keeps the clock \
         right, the system locale (language and formats), and the keyboard layout."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn call(&self, _policy: &Policy, _args: Value) -> Result<Value> {
        let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let td = exec::output("timedatectl", &["show"]).map(|t| parse_keys(&t)).ok();
        let lc = exec::output("localectl", &["status"]).map(|t| parse_status(&t)).ok();
        let yes = |m: &BTreeMap<String, String>, k: &str| m.get(k).map(|v| v == "yes");
        let lang = lc.as_ref().and_then(|l| l.get("System Locale")).and_then(|v| v.split_whitespace().find_map(|p| p.strip_prefix("LANG=")).map(String::from));
        let lang = lang.or_else(|| lang_from(&[Path::new("/etc/locale.conf"), Path::new("/etc/default/locale")]));
        let keyboard = lc.as_ref().and_then(|l| l.get("X11 Layout").or(l.get("VC Keymap"))).cloned().or_else(|| {
            std::fs::read_to_string("/etc/default/keyboard").ok()?.lines().find_map(|l| l.strip_prefix("XKBLAYOUT=").map(|v| v.trim_matches('"').to_string()))
        });
        Ok(json!({
            "local_time": super::journal_time(secs),
            "timezone": td.as_ref().and_then(|t| t.get("Timezone").cloned()).or_else(|| zone_from_link(Path::new("/etc/localtime"))),
            "ntp": td.as_ref().and_then(|t| yes(t, "NTP")),
            "ntp_synchronized": td.as_ref().and_then(|t| yes(t, "NTPSynchronized")),
            "rtc_in_local_time": td.as_ref().and_then(|t| yes(t, "LocalRTC")),
            "locale": lang,
            "keyboard": keyboard,
            "keyboard_variant": lc.as_ref().and_then(|l| l.get("X11 Variant")).cloned(),
            "note": td.is_none().then_some("systemd's timedated isn't reachable here; read from /etc"),
        }))
    }
}

pub struct ClockSet;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetArgs {
    timezone: Option<String>,
    ntp: Option<bool>,
    locale: Option<String>,
    keyboard: Option<String>,
    keyboard_variant: Option<String>,
}

/// The commands, after every value is checked: (program, args).
fn plan(a: &SetArgs) -> Result<Vec<(&'static str, Vec<String>)>> {
    ensure!(a.timezone.is_some() || a.ntp.is_some() || a.locale.is_some() || a.keyboard.is_some(), "say what to change: timezone, ntp, locale or keyboard");
    ensure!(a.keyboard_variant.is_none() || a.keyboard.is_some(), "a keyboard variant goes with a keyboard layout");
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let mut out = Vec::new();
    if let Some(z) = &a.timezone {
        valid_zone(Path::new(ZONEINFO), z)?;
        out.push(("timedatectl", s(&["--no-ask-password", "set-timezone", z])));
    }
    if let Some(n) = a.ntp {
        out.push(("timedatectl", s(&["--no-ask-password", "set-ntp", if n { "true" } else { "false" }])));
    }
    if let Some(l) = &a.locale {
        valid_locale(l, &locales())?;
        out.push(("localectl", s(&["--no-ask-password", "set-locale", &format!("LANG={l}")])));
    }
    if let Some(k) = &a.keyboard {
        valid_keyboard(&std::fs::read_to_string(XKB_RULES).unwrap_or_default(), k, a.keyboard_variant.as_deref())?;
        let mut v = s(&["--no-ask-password", "set-x11-keymap", k]);
        if let Some(var) = &a.keyboard_variant {
            v.extend(s(&["pc105", var]));
        }
        out.push(("localectl", v));
    }
    Ok(out)
}

/// The privileged helper's check of a timedatectl or localectl command:
/// exactly one Orion makes, with values that exist on this machine.
pub(super) fn helper_check(program: &str, args: &[String]) -> Result<()> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match (program, a.as_slice()) {
        ("timedatectl", ["--no-ask-password", "set-timezone", zone]) => valid_zone(Path::new(ZONEINFO), zone),
        ("timedatectl", ["--no-ask-password", "set-ntp", "true" | "false"]) => Ok(()),
        ("localectl", ["--no-ask-password", "set-locale", lang]) => {
            let l = lang.strip_prefix("LANG=").context("only LANG is set")?;
            valid_locale(l, &locales())
        }
        ("localectl", ["--no-ask-password", "set-x11-keymap", layout]) => valid_keyboard(&std::fs::read_to_string(XKB_RULES).unwrap_or_default(), layout, None),
        ("localectl", ["--no-ask-password", "set-x11-keymap", layout, "pc105", variant]) => {
            valid_keyboard(&std::fs::read_to_string(XKB_RULES).unwrap_or_default(), layout, Some(variant))
        }
        _ => bail!("that isn't a {program} command Orion makes"),
    }
}

impl Tool for ClockSet {
    fn name(&self) -> &'static str {
        "clock_set"
    }

    fn description(&self) -> &'static str {
        "Change the system's time zone (Europe/Berlin), turn network time on or off, set the system locale \
         (de_DE.UTF-8: language, dates, numbers) or the keyboard layout (de, optionally with a variant). \
         Locale and layout are the system's defaults; GNOME and KDE keep their own per person, in their settings."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "timezone": { "type": "string", "description": "Like Europe/Berlin or America/New_York." },
                "ntp": { "type": "boolean", "description": "Keep the clock right from the network." },
                "locale": { "type": "string", "description": "Like en_US.UTF-8 or de_DE.UTF-8; it must be generated on the machine." },
                "keyboard": { "type": "string", "description": "An XKB layout: us, gb, de, fr, es…" },
                "keyboard_variant": { "type": "string", "description": "A variant of that layout: nodeadkeys, dvorak, intl…" }
            },
            "additionalProperties": false
        })
    }

    fn preapproval(&self, policy: &Policy, _args: &Value) -> Preapproval {
        policy.desktop.preapproval("clock")
    }

    fn precheck(&self, _policy: &Policy, args: &Value) -> Result<()> {
        plan(&parse_args(args.clone())?).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: SetArgs = parse_args(args)?;
        let mut elevated = false;
        for (program, argv) in plan(&a)? {
            ensure!(exec::installed(program), "{program} isn't here: changing this needs systemd");
            let attempt = |how: Option<&exec::Elevation>| exec::run_as(how, program, &argv, None, Duration::from_secs(30), 16 << 10);
            let mut done = attempt(None)?;
            let denied = done.stderr.to_lowercase().contains("interactive authentication required") || done.stderr.to_lowercase().contains("access denied");
            if !done.success() && denied {
                if let Some(how) = exec::elevation(policy)? {
                    done = attempt(Some(&how))?;
                    elevated = true;
                }
            }
            ensure!(done.success(), "{program} {}: {}", argv[1], done.complaint());
        }
        let mut now = ClockStatus.call(policy, json!({}))?;
        now["ok"] = json!(true);
        now["elevated"] = json!(elevated);
        Ok(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_checks_settings() {
        let t = parse_keys("Timezone=Europe/Berlin\nLocalRTC=no\nCanNTP=yes\nNTP=yes\nNTPSynchronized=yes\n");
        assert_eq!((t["Timezone"].as_str(), t["NTP"].as_str()), ("Europe/Berlin", "yes"));
        let l = parse_status("   System Locale: LANG=de_DE.UTF-8\n                  LC_TIME=en_GB.UTF-8\n       VC Keymap: de-nodeadkeys\n      X11 Layout: de\n     X11 Variant: nodeadkeys\n");
        assert_eq!((l["System Locale"].as_str(), l["X11 Layout"].as_str()), ("LANG=de_DE.UTF-8 LC_TIME=en_GB.UTF-8", "de"));

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Europe")).unwrap();
        std::fs::write(dir.path().join("Europe/Berlin"), "TZif").unwrap();
        assert!(valid_zone(dir.path(), "Europe/Berlin").is_ok());
        for bad in ["Europe", "Europe/../../etc/passwd", "/etc/passwd", "Mars/Olympus", "Europe/Berlin; id", ""] {
            assert!(valid_zone(dir.path(), bad).is_err(), "{bad}");
        }
        std::os::unix::fs::symlink(dir.path().join("Europe/Berlin"), dir.path().join("localtime")).unwrap();
        assert_eq!(zone_from_link(&dir.path().join("localtime")).as_deref(), None); // not under a zoneinfo directory
        std::fs::write(dir.path().join("locale.conf"), "LANG=\"fr_FR.UTF-8\"\n").unwrap();
        assert_eq!(lang_from(&[&dir.path().join("none"), &dir.path().join("locale.conf")]).as_deref(), Some("fr_FR.UTF-8"));

        let have = vec!["C.utf8".to_string(), "de_DE.utf8".into(), "en_US.UTF-8".into()];
        assert!(valid_locale("de_DE.UTF-8", &have).is_ok());
        assert!(valid_locale("ja_JP.UTF-8", &have).is_err());
        assert!(valid_locale("de_DE.UTF-8 LC_ALL=x", &have).is_err());

        let rules = "! model\n  pc105  Generic 105-key PC\n\n! layout\n  us   English (US)\n  de   German\n\n! variant\n  dvorak   us: English (Dvorak)\n  nodeadkeys   de: German (no dead keys)\n";
        assert!(valid_keyboard(rules, "de", Some("nodeadkeys")).is_ok());
        assert!(valid_keyboard(rules, "de", Some("dvorak")).is_err());
        assert!(valid_keyboard(rules, "klingon", None).is_err());
        assert!(valid_keyboard(rules, "-de", None).is_err());

        assert!(helper_check("timedatectl", &["--no-ask-password".into(), "set-ntp".into(), "true".into()]).is_ok());
        assert!(helper_check("timedatectl", &["--no-ask-password".into(), "set-time".into(), "2001-01-01".into()]).is_err());
        assert!(helper_check("localectl", &["--no-ask-password".into(), "set-locale".into(), "LC_ALL=x".into()]).is_err());
    }
}
