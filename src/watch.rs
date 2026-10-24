//! Watching the machine for what needs attention, so Orion can say so
//! before anyone asks: failed services, disks filling up or failing,
//! pending security updates, a reboot that is due, memory running out,
//! parts running hot, a worn battery, bursts of errors and the kernel's
//! hardware complaints in the journal, repeated failed logins, and voice
//! listening that goes wrong (see `voice::tuning`).
//!
//! The daemon runs the checks every `[watch] interval_mins` (updates and
//! disk health every `updates_hours`, since they take a moment), keeps the findings,
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
    /// "disk:/mnt/archive", "updates", "reboot", "memory", "health:sda",
    /// "temp:*", "battery:*", "errors:*", "kernel:*", "logins", "voice:*".
    pub ignore: Vec<String>,
    /// Drives' own SMART data (needs smartctl, and root through the helper).
    pub disk_health: bool,
    /// Parts at or past the point where they get too hot.
    pub temperatures: bool,
    /// A battery that holds less than this share of its charge when new (percent; 0: off).
    pub battery_health_percent: u64,
    /// One program or unit logging this many errors in one interval (0: off).
    /// The kernel's hardware errors (disk I/O, memory, GPU) count from one.
    pub journal_errors: u64,
    /// Failed logins (SSH, sudo, the login screen) in the last hour (0: off).
    pub failed_logins: u64,
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
            disk_health: true,
            temperatures: true,
            battery_health_percent: 70,
            journal_errors: 50,
            failed_logins: 10,
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

/// Drives that report problems in their SMART data.
fn disk_health(policy: &Policy) -> Vec<Finding> {
    if !policy.watch.disk_health {
        return Vec::new();
    }
    let disks = crate::tools::disk_health(policy).unwrap_or_default();
    disks
        .into_iter()
        .filter(|d| d.status == "warning" || d.status == "failing")
        .map(|d| {
            let failing = d.status == "failing";
            let name = d.model.clone().map(|m| format!("{} ({m})", d.device)).unwrap_or(d.device.clone());
            finding(
                format!("health:{}", d.device),
                failing,
                if failing { format!("disk {name} is failing") } else { format!("disk {name} reports problems") },
                d.problems.join("; "),
                if failing { "back up what is on it now, and replace it" } else { "keep backups current; disk_health shows the details" },
            )
        })
        .collect()
}

/// The slow checks, which the daemon runs every `updates_hours`.
fn slow(policy: &Policy) -> Vec<Finding> {
    let mut all = updates(policy);
    all.extend(disk_health(policy));
    all
}

fn temperatures(policy: &Policy) -> Vec<Finding> {
    if !policy.watch.temperatures {
        return Vec::new();
    }
    crate::tools::temperatures()
        .into_iter()
        .filter(|t| t.celsius >= t.hot)
        .map(|t| {
            let what = match t.kind.as_str() {
                "cpu" => "the CPU",
                "gpu" => "a GPU",
                "disk" => "a drive",
                "wifi" => "the Wi-Fi card",
                _ => "a part of the board",
            };
            finding(
                format!("temp:{}:{}", t.source, t.label),
                t.celsius >= t.critical,
                format!("{what} is running hot: {:.0} °C", t.celsius),
                format!("{} {} (too hot from {:.0} °C, critical at {:.0} °C)", t.source, t.label, t.hot, t.critical),
                "sensor_status shows every temperature; check the fans and vents, and what is using the CPU or GPU (proc_list, gpu_info)",
            )
        })
        .collect()
}

fn batteries(percent: u64) -> Vec<Finding> {
    if percent == 0 {
        return Vec::new();
    }
    crate::tools::batteries()
        .into_iter()
        .filter_map(|b| {
            let health = b["health_percent"].as_f64()?;
            (health < percent as f64).then(|| {
                let name = b["name"].as_str().unwrap_or("battery").to_string();
                finding(
                    format!("battery:{name}"),
                    false,
                    format!("the battery holds {health:.0}% of its original charge"),
                    b["cycles"].as_f64().map(|c| format!("{name}, after {c:.0} charge cycles")).unwrap_or(name),
                    "it lasts less on a charge than it did; a replacement brings that back",
                )
            })
        })
        .collect()
}

/// One journal entry, as the checks need it.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    source: String,
    message: String,
}

fn parse_entries(text: &str) -> Vec<Entry> {
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|v| {
            let s = |k: &str| v[k].as_str().filter(|x| !x.is_empty()).map(String::from);
            let message = match &v["MESSAGE"] {
                Value::String(m) => m.clone(),
                Value::Array(bytes) => String::from_utf8_lossy(&bytes.iter().filter_map(|b| b.as_u64().map(|b| b as u8)).collect::<Vec<_>>()).into_owned(),
                _ => String::new(),
            };
            let kernel = v["_TRANSPORT"] == "kernel";
            Entry {
                source: if kernel { "kernel".into() } else { s("_SYSTEMD_UNIT").or(s("_SYSTEMD_USER_UNIT")).or(s("SYSLOG_IDENTIFIER")).or(s("_COMM")).unwrap_or_else(|| "unknown".into()) },
                message,
            }
        })
        .collect()
}

/// `journalctl` entries since `mins` minutes ago, with these filters.
fn journal(mins: u64, filters: &[&str]) -> Vec<Entry> {
    let since = format!("--since=-{mins}min");
    let mut args = vec!["--no-pager", "--quiet", "-o", "json", "--output-fields=MESSAGE,_SYSTEMD_UNIT,_SYSTEMD_USER_UNIT,SYSLOG_IDENTIFIER,_COMM,_TRANSPORT", since.as_str()];
    args.extend(filters);
    crate::tools::exec::output("journalctl", &args).map(|t| parse_entries(&t)).unwrap_or_default()
}

fn short(m: &str) -> String {
    let m = m.trim();
    if m.chars().count() > 160 { format!("{}…", m.chars().take(160).collect::<String>()) } else { m.to_string() }
}

/// Programs or units that logged at least `threshold` errors.
fn error_bursts(entries: &[Entry], threshold: u64, mins: u64) -> Vec<Finding> {
    let mut by: BTreeMap<&str, (u64, &str)> = BTreeMap::new();
    for e in entries.iter().filter(|e| e.source != "kernel") {
        let c = by.entry(e.source.as_str()).or_insert((0, e.message.as_str()));
        c.0 += 1;
        c.1 = e.message.as_str();
    }
    by.into_iter()
        .filter(|(_, (n, _))| *n >= threshold)
        .map(|(source, (n, last))| {
            finding(
                format!("errors:{source}"),
                false,
                format!("{source} logged {n} errors in {mins} minutes"),
                format!("the last: {}", short(last)),
                "journal_read with the unit and priority err shows them",
            )
        })
        .collect()
}

/// The kernel's complaints about hardware and filesystems: each counts.
const KERNEL: [(&str, bool, &str, &[&str]); 6] = [
    ("kernel:hardware", true, "the CPU or memory reported hardware errors", &["Hardware Error", "Machine check", "EDAC", "mce: [Hardware"]),
    ("kernel:filesystem", true, "a filesystem reported errors", &["EXT4-fs error", "BTRFS error", "XFS (", "corrupt"]),
    ("kernel:io", false, "a disk reported read or write errors", &["I/O error", "critical medium error", "failed command: READ", "failed command: WRITE"]),
    ("kernel:gpu", false, "the GPU crashed or was reset", &["fallen off the bus", "GPU reset", "ring gfx timeout", "NVRM: Xid", "GPU HANG"]),
    ("kernel:throttle", false, "the CPU slowed itself down to cool off", &["temperature above threshold", "clock throttled"]),
    ("kernel:oom", false, "memory ran out and the kernel ended a program", &["Out of memory: Killed process", "oom-kill:"]),
];

fn kernel_problems(entries: &[Entry]) -> Vec<Finding> {
    KERNEL
        .iter()
        .filter_map(|(key, critical, title, patterns)| {
            let hits: Vec<&Entry> = entries.iter().filter(|e| e.source == "kernel" && patterns.iter().any(|p| e.message.contains(p))).collect();
            // "XFS (" alone is any XFS message: it must say corruption or error too.
            let hits: Vec<&Entry> = hits.into_iter().filter(|e| *key != "kernel:filesystem" || !e.message.starts_with("XFS (") || e.message.contains("rror") || e.message.contains("orrupt")).collect();
            let last = hits.last()?;
            Some(finding(
                key.to_string(),
                *critical,
                title.to_string(),
                format!("{} time{}; the last: {}", hits.len(), if hits.len() == 1 { "" } else { "s" }, short(&last.message)),
                "journal_read with kernel shows them; disk_health and sensor_status may say more",
            ))
        })
        .collect()
}

const FAILED: [&str; 5] = ["Failed password for", "Invalid user ", "authentication failure", "maximum authentication attempts", "FAILED SU"];

/// Repeated failed logins, and where most came from.
fn failed_logins(entries: &[Entry], threshold: u64) -> Vec<Finding> {
    let failed: Vec<&Entry> = entries.iter().filter(|e| FAILED.iter().any(|p| e.message.contains(p))).collect();
    if threshold == 0 || (failed.len() as u64) < threshold {
        return Vec::new();
    }
    let mut from: BTreeMap<String, usize> = BTreeMap::new();
    for e in &failed {
        // sshd: "… from 203.0.113.9 port 51522 ssh2"
        if let Some(addr) = e.message.split(" from ").nth(1).and_then(|r| r.split_whitespace().next()) {
            *from.entry(addr.to_string()).or_default() += 1;
        }
    }
    let top = from.iter().max_by_key(|(_, n)| **n);
    let programs: BTreeSet<&str> = failed.iter().map(|e| e.source.as_str()).collect();
    vec![finding(
        "logins",
        false,
        format!("{} failed logins in the last hour", failed.len()),
        match top {
            Some((addr, n)) => format!("{n} from {addr}; through {}", programs.into_iter().collect::<Vec<_>>().join(", ")),
            None => format!("through {}", programs.into_iter().collect::<Vec<_>>().join(", ")),
        },
        "someone may be guessing passwords: fw_rule can block an address, and SSH is safer with keys only",
    )]
}

fn journal_checks(policy: &Policy) -> Vec<Finding> {
    let w = &policy.watch;
    let mut all = Vec::new();
    if w.journal_errors > 0 {
        let mins = w.interval_mins.max(1);
        all.extend(error_bursts(&journal(mins, &["--priority=err"]), w.journal_errors, mins));
        all.extend(kernel_problems(&journal(mins, &["--dmesg", "--priority=warning"])));
    }
    if w.failed_logins > 0 {
        // The auth and authpriv facilities: sshd, sudo, su, PAM.
        all.extend(failed_logins(&journal(60, &["SYSLOG_FACILITY=4", "SYSLOG_FACILITY=10"]), w.failed_logins));
    }
    all
}

/// Run the checks: the quick ones always; the update list and disk health when asked.
/// Listening that went wrong: noise keeping requests going, the wake word
/// heard as something else. Each comes with the setting that fixes it.
fn voice(policy: &Policy) -> Vec<Finding> {
    let log = crate::voice::tuning::VoiceLog::load();
    crate::voice::tuning::diagnose(&log, &policy.voice)
        .into_iter()
        .map(|s| {
            let hint = if s.change.as_object().is_some_and(|c| !c.is_empty()) {
                format!("voice_tune {} fixes it, with the person's go-ahead; voice_check has the details", s.change)
            } else {
                "voice_check has the details".to_string()
            };
            Finding { key: s.key, severity: "warning".into(), title: s.title, detail: s.why, hint, since: None }
        })
        .collect()
}

pub fn check(policy: &Policy, with_updates: bool) -> Vec<Finding> {
    let w = &policy.watch;
    let mut all = services();
    all.extend(disks(w.disk_percent));
    all.extend(memory(w.memory_percent));
    all.extend(reboot(policy));
    all.extend(temperatures(policy));
    all.extend(batteries(w.battery_health_percent));
    all.extend(journal_checks(policy));
    all.extend(voice(policy));
    if with_updates {
        all.extend(slow(policy));
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
        // A checkpoint of the audit log with the helper, when it grew.
        if let Err(e) = crate::audit::anchor(orion.policy()) {
            tracing::warn!("audit checkpoint: {e:#}");
        }
        let (unattended, ran) = unattended_round(&orion, &mut last, &health);
        if ran {
            updates_at = None; // what is still pending has changed
        }
        if cfg.enabled {
            let due = updates_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(cfg.updates_hours.max(1) * 3600));
            if due {
                updates_found = slow(orion.policy());
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
        "What on this machine needs attention now: failed services, filesystems nearly full or drives failing, \
         pending security updates, a reboot that is due, memory running out, parts running hot, a worn battery, \
         bursts of errors or hardware errors in the journal, repeated failed logins. Each finding says what to do \
         about it. Check it when asked how the machine is doing, or before big changes."
    }

    fn risk(&self) -> crate::tool::Risk {
        crate::tool::Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "updates": { "type": "boolean", "default": true, "description": "Also list pending updates and read the drives' SMART data (takes a moment)." }
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
    fn reads_the_journal() {
        let line = |transport: &str, unit: &str, msg: &str| {
            json!({ "MESSAGE": msg, "_TRANSPORT": transport, "_SYSTEMD_UNIT": unit, "SYSLOG_IDENTIFIER": "x" }).to_string()
        };
        let mut text = String::new();
        for i in 0..60 {
            text.push_str(&line("journal", "nginx.service", &format!("upstream timed out {i}")));
            text.push('\n');
        }
        text.push_str(&line("stdout", "cron.service", "one error"));
        text.push('\n');
        // A message that isn't text arrives as bytes.
        text.push_str(&json!({ "MESSAGE": [104, 105], "SYSLOG_IDENTIFIER": "app" }).to_string());
        let e = parse_entries(&text);
        assert_eq!((e.len(), e[61].source.as_str(), e[61].message.as_str()), (62, "app", "hi"));
        let b = error_bursts(&e, 50, 15);
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].key.as_str(), b[0].title.as_str()), ("errors:nginx.service", "nginx.service logged 60 errors in 15 minutes"));
        assert!(b[0].detail.contains("upstream timed out 59"));

        let k = parse_entries(&[
            line("kernel", "", "blk_update_request: I/O error, dev sda, sector 123456 op 0x0:(READ) flags 0x0"),
            line("kernel", "", "XFS (sdb1): Mounting V5 Filesystem"),
            line("kernel", "", "mce: [Hardware Error]: Machine check events logged"),
            line("kernel", "", "NVRM: Xid (PCI:0000:01:00): 79, pid=1234, GPU has fallen off the bus."),
            line("journal", "app.service", "I/O error in my own code"),
        ].join("\n"));
        let found = kernel_problems(&k);
        let keys: Vec<&str> = found.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["kernel:hardware", "kernel:io", "kernel:gpu"]);
        assert_eq!(found[0].severity, "critical");
        assert!(found[1].detail.starts_with("1 time; the last: blk_update_request"));

        let logins: Vec<String> = (0..12)
            .map(|i| line("syslog", "ssh.service", &format!("Failed password for invalid user admin from 203.0.113.{} port 5{i} ssh2", if i < 9 { 9 } else { 7 })))
            .chain(["pam_unix(sudo:auth): authentication failure; logname=ana uid=1000".to_string()].map(|m| line("syslog", "", &m)))
            .collect();
        let l = failed_logins(&parse_entries(&logins.join("\n")), 10);
        assert_eq!((l.len(), l[0].title.as_str()), (1, "13 failed logins in the last hour"));
        assert!(l[0].detail.starts_with("9 from 203.0.113.9"), "{}", l[0].detail);
        assert!(failed_logins(&parse_entries(&logins[..3].join("\n")), 10).is_empty());
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
