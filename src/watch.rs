//! Watching the machine for what needs attention, so Orion can say so
//! before anyone asks: failed services, disks filling up, pending security
//! updates, a reboot that is due, memory running out.
//!
//! The daemon runs the checks every `[watch] interval_mins` (updates every
//! `updates_hours`, since listing them takes a moment), keeps the findings,
//! and notifies the desktop the first time each one appears. `orion
//! status`, the `system_health` tool, the chat and the shell show them.
//! Without a daemon, the interfaces run the quick checks themselves.
//!
//! The daemon also installs updates unattended, once a day, when `[packages]
//! automatic` asks for it: through `update_apply`, under the policy like any
//! other call (so only as far as `packages.updates` lets it run without a
//! person), with snapshots and a history entry, and never on battery unless
//! allowed. It never reboots; the reboot check says when one is due.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::policy::Policy;

/// `[watch]` in orion.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WatchConfig {
    /// The daemon watches at all.
    pub enabled: bool,
    pub interval_mins: u64,
    /// Pending updates are listed this often (it takes a moment).
    pub updates_hours: u64,
    /// A filesystem this full (percent of what can be used) needs attention.
    pub disk_percent: u64,
    /// Memory needs attention below this share available (percent).
    pub memory_percent: u64,
    /// A desktop notification the first time each finding appears.
    pub notify: bool,
    /// Findings to leave out, by key (`*` wildcards): "service:backup.service",
    /// "disk:/mnt/archive", "updates", "reboot", "memory".
    pub ignore: Vec<String>,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_mins: 15,
            updates_hours: 6,
            disk_percent: 90,
            memory_percent: 5,
            notify: true,
            ignore: Vec::new(),
        }
    }
}

/// One thing that needs attention.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Finding {
    /// Stable while the problem lasts: "disk:/home", "service:nginx.service".
    pub key: String,
    /// "warning" or "critical".
    pub severity: String,
    pub title: String,
    pub detail: String,
    /// What to do about it, in Orion's terms.
    pub hint: String,
    /// When it was first seen ("YYYY-MM-DD HH:MM:SS"), by the daemon.
    #[serde(default)]
    pub since: Option<String>,
}

fn finding(key: impl Into<String>, critical: bool, title: String, detail: String, hint: &str) -> Finding {
    Finding {
        key: key.into(),
        severity: if critical { "critical" } else { "warning" }.into(),
        title,
        detail,
        hint: hint.into(),
        since: None,
    }
}

/// Failed services: the machine's, and your own user services'.
fn services() -> Vec<Finding> {
    let mut out = Vec::new();
    for unit in crate::tools::failed_units().unwrap_or_default() {
        out.push(finding(
            format!("service:{unit}"),
            false,
            format!("{unit} failed"),
            "a system service stopped with an error".into(),
            "svc_status shows why; svc_control restarts it",
        ));
    }
    let user = crate::tools::exec::output("systemctl", &["--user", "list-units", "--state=failed", "--plain", "--no-legend", "--no-pager"]);
    for unit in user.unwrap_or_default().lines().filter_map(|l| l.split_whitespace().next()) {
        out.push(finding(
            format!("service:{unit}"),
            false,
            format!("{unit} failed"),
            "one of your own services stopped with an error".into(),
            "svc_status with scope user shows why",
        ));
    }
    out
}

/// The filesystems worth watching in `/proc/self/mounts`: real ones,
/// writable, one mount point per device. (device, mount point).
fn mounts(text: &str) -> Vec<(String, String)> {
    const REAL: [&str; 15] =
        ["ext2", "ext3", "ext4", "xfs", "btrfs", "f2fs", "vfat", "exfat", "ntfs", "ntfs3", "zfs", "jfs", "bcachefs", "nilfs2", "fuseblk"];
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let [dev, at, fstype, opts, ..] = f[..] else { continue };
        if !REAL.contains(&fstype) || opts.split(',').any(|o| o == "ro") || !seen.insert(dev.to_string()) {
            continue;
        }
        out.push((dev.to_string(), unescape_mount(at)));
    }
    out
}

/// `/mnt/My\040Disk` → `/mnt/My Disk`: the kernel escapes spaces and such.
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push(u8::from_str_radix(&s[i + 1..i + 4], 8).unwrap_or(b'?'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// (percent used of what can be used, bytes free), as `df` counts them.
fn usage(at: &str) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(at).ok()?;
    // SAFETY: a zeroed statvfs is a valid output buffer.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid C string and `s` a properly sized buffer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 || s.f_blocks == 0 {
        return None;
    }
    let used = s.f_blocks.saturating_sub(s.f_bfree);
    let usable = used + s.f_bavail;
    Some(((used * 100).div_ceil(usable.max(1)), s.f_bavail * s.f_frsize))
}

fn disks(percent: u64) -> Vec<Finding> {
    let text = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    mounts(&text)
        .into_iter()
        .filter_map(|(_, at)| {
            let (pct, free) = usage(&at)?;
            (pct >= percent).then(|| {
                finding(
                    format!("disk:{at}"),
                    pct >= 98,
                    format!("{at} is {pct}% full"),
                    format!("{} free", gib(free)),
                    "disk_usage shows what fills it; fs_delete moves things to the Trash",
                )
            })
        })
        .collect()
}

fn gib(bytes: u64) -> String {
    let g = bytes as f64 / (1u64 << 30) as f64;
    if g >= 10.0 { format!("{g:.0} GiB") } else if g >= 1.0 { format!("{g:.1} GiB") } else { format!("{} MiB", bytes >> 20) }
}

fn memory(percent: u64) -> Vec<Finding> {
    let info = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |k: &str| info.lines().find_map(|l| l.strip_prefix(k)).and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok());
    let (Some(total), Some(avail)) = (field("MemTotal:"), field("MemAvailable:")) else { return Vec::new() };
    let pct = avail * 100 / total.max(1);
    if pct >= percent {
        return Vec::new();
    }
    vec![finding(
        "memory",
        pct < 2,
        format!("memory is running out: {pct}% available"),
        format!("{} of {} available", gib(avail << 10), gib(total << 10)),
        "proc_list sorted by memory shows what uses it",
    )]
}

fn reboot(policy: &Policy) -> Vec<Finding> {
    let why = crate::tools::reboot_needed(policy);
    if why.is_empty() {
        return Vec::new();
    }
    vec![finding("reboot", false, "a reboot is due".into(), why.join("; "), "reboot when it suits you; Orion doesn't reboot on its own")]
}

fn updates(policy: &Policy) -> Vec<Finding> {
    match crate::tools::pending_updates(policy) {
        Ok((_, security, kernel)) if security > 0 => vec![finding(
            "updates",
            false,
            format!("{security} security update{} pending", if security == 1 { "" } else { "s" }),
            if kernel { "a new kernel among the updates".into() } else { String::new() },
            "update_apply with security_only installs them",
        )],
        _ => Vec::new(),
    }
}

/// Run the checks: the quick ones always, the update list when asked.
pub fn check(policy: &Policy, with_updates: bool) -> Vec<Finding> {
    let w = &policy.watch;
    let mut all = services();
    all.extend(disks(w.disk_percent));
    all.extend(memory(w.memory_percent));
    all.extend(reboot(policy));
    if with_updates {
        all.extend(updates(policy));
    }
    all.retain(|f| !w.ignore.iter().any(|p| crate::tools::exec::glob_match(p, &f.key)));
    all
}

/// What the daemon knows, shared with its connections.
#[derive(Default, Serialize, Clone)]
pub struct Health {
    pub findings: Vec<Finding>,
    pub checked_at: Option<String>,
    pub updates_checked_at: Option<String>,
    /// The latest unattended update run.
    pub unattended: Option<Unattended>,
}

/// One unattended update run, as `$XDG_STATE_HOME/orion/unattended.json`
/// keeps the latest.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Unattended {
    /// Local date and time it ran.
    pub at: String,
    pub ok: bool,
    pub summary: String,
    /// Its entry in the package history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<u64>,
}

fn unattended_path() -> Option<PathBuf> {
    Policy::state_dir().map(|d| d.join("unattended.json"))
}

/// The latest unattended update run, if there was one.
pub fn last_unattended() -> Option<Unattended> {
    serde_json::from_str(&std::fs::read_to_string(unattended_path()?).ok()?).ok()
}

/// `HH:MM` as minutes after midnight.
fn minutes(hhmm: &str) -> Option<u32> {
    let (h, m) = hhmm.split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60 && hhmm.len() == 5).then_some(h * 60 + m)
}

/// `[packages] automatic`: the update_apply arguments and the time of
/// day, `None` when off, or what is wrong with the settings.
fn automatic(policy: &Policy) -> Result<Option<(Value, u32)>, String> {
    let p = &policy.packages;
    let args = match p.automatic.as_str() {
        "off" => return Ok(None),
        "security" => json!({ "security_only": true }),
        "all" => json!({}),
        other => return Err(format!("packages.automatic must be off, security or all, not {other:?}")),
    };
    let at = minutes(&p.automatic_at).ok_or_else(|| format!("packages.automatic_at must be a time like \"04:00\", not {:?}", p.automatic_at))?;
    Ok(Some((args, at)))
}

/// Whether the day's run is due at `now` (local "YYYY-MM-DD HH:MM:SS"):
/// the time has come, and it hasn't run today.
fn due(at: u32, now: &str, last: Option<&Unattended>) -> bool {
    let Some((date, time)) = now.split_once(' ') else { return false };
    let Some(mins) = time.get(..5).and_then(minutes) else { return false };
    mins >= at && last.is_none_or(|l| !l.at.starts_with(date))
}

/// Running on battery: a battery discharging.
fn on_battery() -> bool {
    let Ok(dir) = std::fs::read_dir("/sys/class/power_supply") else { return false };
    dir.flatten().any(|e| {
        let read = |f: &str| std::fs::read_to_string(e.path().join(f)).unwrap_or_default();
        read("type").trim() == "Battery" && read("status").trim() == "Discharging"
    })
}

/// What unattended updates are set to do, for `orion status`.
pub fn describe_automatic(policy: &Policy, last: Option<&Unattended>) -> String {
    let mut out = match automatic(policy) {
        Ok(None) => "unattended: off ([packages] automatic)".to_string(),
        Ok(Some(_)) => format!(
            "unattended: {}, daily from {}{}",
            if policy.packages.automatic == "security" { "security fixes" } else { "all updates" },
            policy.packages.automatic_at,
            if policy.packages.automatic_on_battery { "" } else { ", not on battery" }
        ),
        Err(e) => format!("unattended: misconfigured: {e}"),
    };
    if let Some(l) = last {
        let change = l.change.map(|c| format!(" (change {c})")).unwrap_or_default();
        out.push_str(&format!("; last {}: {}{change}", &l.at[..l.at.len().min(16)], l.summary));
    }
    out
}

/// Refresh the lists and install the updates, as the policy allows.
fn run_unattended(orion: &crate::Orion, args: Value) -> Unattended {
    let caller = Some("unattended updates");
    let at = now();
    let fail = |summary: String, change: Option<u64>| Unattended { at: at.clone(), ok: false, summary, change };
    if let Err(e) = orion.call_as("update_check", json!({ "refresh": true }), None, caller) {
        return fail(format!("checking for updates: {e}"), None);
    }
    match orion.call_as("update_apply", args, None, caller) {
        Ok(v) if v["ok"] == false => fail(v["error"].as_str().unwrap_or("the update failed").to_string(), v["change"].as_u64()),
        Ok(v) => {
            let n = v["upgraded"].as_array().map_or(0, Vec::len);
            let summary = match n {
                0 => "nothing to update".to_string(),
                1 => "installed 1 update".to_string(),
                n => format!("installed {n} updates"),
            };
            Unattended { at: at.clone(), ok: true, summary, change: v["change"].as_u64() }
        }
        Err(e) => fail(e.to_string(), None),
    }
}

/// The day's unattended updates, when due. What needs attention about them.
fn unattended_round(orion: &crate::Orion, last: &mut Option<Unattended>, health: &Shared) -> (Vec<Finding>, bool) {
    let policy = orion.policy();
    let mut ran = false;
    match automatic(policy) {
        Err(why) => {
            return (vec![finding("unattended", false, "Unattended updates are misconfigured".to_string(), why, "fix [packages] in orion.toml")], false);
        }
        Ok(Some((args, at))) if due(at, &now(), last.as_ref()) && (policy.packages.automatic_on_battery || !on_battery()) => {
            let run = run_unattended(orion, args);
            if let Some(path) = unattended_path() {
                let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path)).and_then(|()| std::fs::write(&path, json!(run).to_string()));
            }
            if run.ok && run.change.is_some() && policy.watch.notify {
                let undo = run.change.map(|c| format!("pkg_rollback with id {c} puts them back")).unwrap_or_default();
                notify_text(&format!("Orion {}", run.summary), &undo, false);
            }
            health.lock().unwrap_or_else(|e| e.into_inner()).unattended = Some(run.clone());
            *last = Some(run);
            ran = true;
        }
        Ok(_) => {}
    }
    let mut found = Vec::new();
    if let Some(l) = last.as_ref().filter(|l| !l.ok) {
        found.push(finding(
            "unattended",
            false,
            format!("Unattended updates failed ({})", &l.at[..l.at.len().min(16)]),
            l.summary.clone(),
            "they try again tomorrow; update_apply shows what goes wrong",
        ));
    }
    (found, ran)
}

pub type Shared = Arc<Mutex<Health>>;

fn now() -> String {
    let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    crate::tools::journal_time(secs)
}

/// The daemon's watch, until the process ends. `policy` gives the current
/// policy each round, so edits apply.
pub fn run(policy: impl Fn() -> Arc<crate::Orion>, health: Shared) {
    let mut first_seen: BTreeMap<String, String> = BTreeMap::new();
    let mut updates_found: Vec<Finding> = Vec::new();
    let mut updates_at: Option<Instant> = None;
    let mut last = last_unattended();
    health.lock().unwrap_or_else(|e| e.into_inner()).unattended = last.clone();
    loop {
        let orion = policy();
        let cfg = orion.policy().watch.clone();
        let (unattended, ran) = unattended_round(&orion, &mut last, &health);
        if ran {
            updates_at = None; // what is still pending has changed
        }
        if cfg.enabled {
            let due = updates_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(cfg.updates_hours.max(1) * 3600));
            if due {
                updates_found = updates(orion.policy());
                updates_at = Some(Instant::now());
                health.lock().unwrap_or_else(|e| e.into_inner()).updates_checked_at = Some(now());
            }
            let mut found = check(orion.policy(), false);
            found.extend(unattended.into_iter().filter(|f| !cfg.ignore.iter().any(|p| crate::tools::exec::glob_match(p, &f.key))));
            found.extend(updates_found.iter().filter(|f| !cfg.ignore.iter().any(|p| crate::tools::exec::glob_match(p, &f.key))).cloned());
            let stamp = now();
            for f in &mut found {
                let since = first_seen.entry(f.key.clone()).or_insert_with(|| {
                    // New: tell the person, once.
                    if cfg.notify {
                        notify(f);
                    }
                    stamp.clone()
                });
                f.since = Some(since.clone());
            }
            // Gone problems are forgotten, so they notify again if they return.
            let keys: BTreeSet<&String> = found.iter().map(|f| &f.key).collect();
            first_seen.retain(|k, _| keys.contains(k));
            let mut h = health.lock().unwrap_or_else(|e| e.into_inner());
            h.findings = found;
            h.checked_at = Some(stamp);
        }
        std::thread::sleep(Duration::from_secs(cfg.interval_mins.max(1) * 60));
    }
}

fn notify(f: &Finding) {
    let body = [f.detail.as_str(), f.hint.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" · ");
    notify_text(&f.title, &body, f.severity == "critical");
}

fn notify_text(title: &str, body: &str, critical: bool) {
    if !crate::tools::exec::installed("notify-send") {
        return;
    }
    let urgency = if critical { "--urgency=critical" } else { "--urgency=normal" };
    let _ = std::process::Command::new("notify-send")
        .args(["--app-name=Orion", urgency, "--", title, body])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// The daemon's latest findings, if it runs.
pub fn from_daemon(config: &crate::daemon::DaemonConfig) -> Option<Health> {
    use std::io::{BufRead, BufReader, Write};
    let stream = crate::daemon::connect(config)?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    let mut w = &stream;
    writeln!(w, "{}", json!({ "jsonrpc": "2.0", "id": 1, "method": "orion/health" })).ok()?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).ok()?;
    let v: Value = serde_json::from_str(&line).ok()?;
    let r = v.get("result")?;
    Some(Health {
        findings: serde_json::from_value(r["findings"].clone()).ok()?,
        checked_at: r["checked_at"].as_str().map(String::from),
        updates_checked_at: r["updates_checked_at"].as_str().map(String::from),
        unattended: serde_json::from_value(r["unattended"].clone()).ok(),
    })
}

/// What needs attention now, for an interface: the daemon's findings if it
/// runs, else the quick checks here.
pub fn current(policy: &Policy) -> Vec<Finding> {
    match from_daemon(&policy.daemon) {
        Some(h) => h.findings,
        None => check(policy, false),
    }
}

pub struct SystemHealth;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HealthArgs {
    #[serde(default = "yes")]
    updates: bool,
}

fn yes() -> bool {
    true
}

impl crate::Tool for SystemHealth {
    fn name(&self) -> &'static str {
        "system_health"
    }

    fn description(&self) -> &'static str {
        "What on this machine needs attention now: failed services, filesystems nearly full, pending security \
         updates, a reboot that is due, memory running out. Each finding says what to do about it. Check it when \
         asked how the machine is doing, or before big changes."
    }

    fn risk(&self) -> crate::tool::Risk {
        crate::tool::Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "updates": { "type": "boolean", "default": true, "description": "Also list pending updates (takes a moment)." }
            },
            "additionalProperties": false
        })
    }

    fn call(&self, policy: &Policy, args: Value) -> anyhow::Result<Value> {
        let a: HealthArgs = crate::tool::parse_args(args)?;
        let found = check(policy, a.updates);
        Ok(json!({
            // Not "ok": that means the call itself failed (crate::reported_failure).
            "healthy": found.is_empty(),
            "summary": if found.is_empty() { "nothing needs attention".to_string() } else { format!("{} thing{} need{} attention", found.len(), if found.len() == 1 { "" } else { "s" }, if found.len() == 1 { "s" } else { "" }) },
            "findings": found,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_filesystems_worth_watching() {
        let m = mounts(
            "/dev/nvme0n1p2 / ext4 rw,relatime 0 0\n\
             /dev/nvme0n1p1 /boot/efi vfat rw 0 0\n\
             /dev/nvme0n1p3 /home btrfs rw,subvol=/home 0 0\n\
             /dev/nvme0n1p3 /home/.snapshots btrfs rw,subvol=/snap 0 0\n\
             tmpfs /run tmpfs rw 0 0\n\
             /dev/loop3 /snap/core22/1380 squashfs ro 0 0\n\
             /dev/sdb1 /media/me/My\\040Photos exfat rw 0 0\n\
             /dev/sdc1 /mnt/archive ext4 ro 0 0\n",
        );
        let at: Vec<&str> = m.iter().map(|(_, a)| a.as_str()).collect();
        assert_eq!(at, ["/", "/boot/efi", "/home", "/media/me/My Photos"]);
        assert_eq!(unescape_mount("/a\\040b\\011c"), "/a b\tc");
        assert_eq!(unescape_mount("/plain\\"), "/plain\\");
        let (pct, free) = usage("/").unwrap();
        assert!(pct <= 100 && free > 0);
        assert_eq!((gib(5 << 30), gib(64 << 30), gib(300 << 20)), ("5.0 GiB".into(), "64 GiB".into(), "300 MiB".into()));
    }

    #[test]
    fn checks_respect_thresholds_and_ignores() {
        // Every real filesystem here is at least 0% full.
        let everything = Policy::from_toml("[watch]\ndisk_percent = 0\n").unwrap();
        let found = check(&everything, false);
        assert!(found.iter().any(|f| f.key == "disk:/"), "{found:?}");
        let quiet = Policy::from_toml("[watch]\ndisk_percent = 0\nignore = [\"disk:*\"]\n").unwrap();
        assert!(!check(&quiet, false).iter().any(|f| f.key.starts_with("disk:")));
        // Memory: nothing has 101% available.
        assert_eq!(memory(101).len(), 1);
        assert!(memory(0).is_empty());
    }

    #[test]
    fn unattended_updates_run_once_a_day() {
        assert_eq!((minutes("04:00"), minutes("23:59"), minutes("24:00"), minutes("4:00"), minutes("x")), (Some(240), Some(1439), None, None, None));
        let ran = |at: &str| Unattended { at: at.into(), ok: true, summary: String::new(), change: None };
        assert!(!due(240, "2026-10-23 03:59:59", None));
        assert!(due(240, "2026-10-23 04:00:00", None));
        assert!(due(240, "2026-10-23 13:30:00", Some(&ran("2026-10-22 04:00:10"))), "catches up after the machine was off");
        assert!(!due(240, "2026-10-23 13:30:00", Some(&ran("2026-10-23 04:00:10"))));
        let p = |t: &str| Policy::from_toml(t).unwrap();
        assert_eq!(automatic(&p("")), Ok(None));
        assert_eq!(automatic(&p("[packages]\nautomatic = \"security\"\n")), Ok(Some((json!({ "security_only": true }), 240))));
        assert!(automatic(&p("[packages]\nautomatic = \"all\"\nautomatic_at = \"25:00\"\n")).unwrap_err().contains("automatic_at"));
        assert!(automatic(&p("[packages]\nautomatic = \"yes\"\n")).is_err());
        let d = describe_automatic(&p("[packages]\nautomatic = \"security\"\n"), Some(&ran("2026-10-23 04:00:10")));
        assert_eq!(d, "unattended: security fixes, daily from 04:00, not on battery; last 2026-10-23 04:00: ");
    }
}
