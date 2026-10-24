//! Disk health, from each drive's own SMART data (`smartctl --json`, from
//! smartmontools): whether the drive says it is failing, bad or pending
//! sectors, uncorrectable errors, an SSD's wear, and its temperature.
//!
//! Reading SMART needs root, so it goes through the privileged helper (or
//! sudo), which runs only `smartctl --json=c -a --nocheck=standby` on a
//! whole disk in /sys/block. Sleeping disks are left asleep.

use std::path::Path;
use std::time::Duration;

use anyhow::{ensure, Result};
use serde::Serialize;
use serde_json::{json, Value};

use super::exec;
use crate::policy::Policy;
use crate::tool::{Risk, Tool};

/// One drive's health, as Orion sums it up.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Disk {
    pub device: String,
    pub model: Option<String>,
    /// "good", "warning", "failing", or "unknown" (no SMART data).
    pub status: String,
    /// Why it isn't good.
    pub problems: Vec<String>,
    pub temperature: Option<i64>,
    pub power_on_hours: Option<i64>,
    /// An SSD's wear, percent of its rated life used.
    pub wear_percent: Option<i64>,
}

/// The whole disks under `root` (/sys/block): no partitions, loop, RAM or
/// optical devices, nor device-mapper and RAID layers on top of disks.
fn devices(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };
    let mut out: Vec<String> = entries.flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|n| disk_name(n)).collect();
    out.sort();
    out
}

fn disk_name(n: &str) -> bool {
    let kinds = ["sd", "nvme", "vd", "hd", "xvd", "mmcblk"];
    kinds.iter().any(|k| n.starts_with(k)) && n.len() <= 32 && n.chars().all(|c| c.is_ascii_alphanumeric())
}

fn args(device: &str) -> Vec<String> {
    ["--json=c", "-a", "--nocheck=standby"].iter().map(|s| s.to_string()).chain([format!("/dev/{device}")]).collect()
}

/// The privileged helper's check: exactly Orion's smartctl command, on a
/// whole disk that exists.
pub(super) fn helper_check(args_given: &[String]) -> Result<()> {
    let device = args_given.last().and_then(|d| d.strip_prefix("/dev/")).unwrap_or_default();
    ensure!(disk_name(device) && Path::new("/sys/block").join(device).exists(), "that isn't a disk on this machine");
    ensure!(args_given == args(device).as_slice(), "that isn't a smartctl command Orion makes");
    Ok(())
}

/// A SMART attribute's raw value, by id.
fn raw(attrs: &[Value], id: u64) -> Option<i64> {
    attrs.iter().find(|a| a["id"] == id).and_then(|a| a["raw"]["value"].as_i64())
}

/// What `smartctl --json` says about `device`.
fn assess(device: &str, v: &Value) -> Disk {
    let mut d = Disk {
        device: device.into(),
        model: v["model_name"].as_str().or(v["model_family"].as_str()).map(String::from),
        status: "good".into(),
        problems: Vec::new(),
        temperature: v["temperature"]["current"].as_i64(),
        power_on_hours: v["power_on_time"]["hours"].as_i64(),
        wear_percent: None,
    };
    let Some(passed) = v["smart_status"]["passed"].as_bool() else {
        d.status = "unknown".into();
        let why = v["smartctl"]["messages"].as_array().and_then(|m| m.iter().find_map(|x| x["string"].as_str())).unwrap_or("no SMART data");
        d.problems.push(why.trim_start_matches(&format!("/dev/{device}: ")).to_string());
        return d;
    };
    let mut failing = !passed;
    if !passed {
        d.problems.push("the drive's own self-assessment says it is failing".into());
    }
    let mut warn = |p: String| d.problems.push(p);
    // ATA drives: sectors moved, waiting, or lost.
    let attrs = v["ata_smart_attributes"]["table"].as_array().cloned().unwrap_or_default();
    for (id, what) in [(5, "reallocated sectors"), (197, "sectors waiting to be reallocated"), (198, "sectors it couldn't read"), (187, "uncorrectable errors")] {
        if let Some(n) = raw(&attrs, id).filter(|n| *n > 0) {
            warn(format!("{n} {what}"));
        }
    }
    for a in &attrs {
        if a["when_failed"] == "now" {
            failing = true;
            warn(format!("{} is past its failure threshold", a["name"].as_str().unwrap_or("an attribute")));
        }
    }
    // An SATA SSD's remaining life, as the vendors' attributes count it (100 = new).
    let life = attrs.iter().find(|a| [231, 233, 177, 202].contains(&a["id"].as_u64().unwrap_or(0))).and_then(|a| a["value"].as_i64());
    // NVMe: its health log.
    let n = &v["nvme_smart_health_information_log"];
    if n.is_object() {
        if n["critical_warning"].as_i64().is_some_and(|c| c != 0) {
            failing = true;
            warn(format!("the drive raised a critical warning (code {})", n["critical_warning"]));
        }
        if let (Some(spare), Some(threshold)) = (n["available_spare"].as_i64(), n["available_spare_threshold"].as_i64()) {
            if spare < threshold {
                failing = true;
                warn(format!("spare capacity is down to {spare}% (the drive's limit is {threshold}%)"));
            }
        }
        if let Some(e) = n["media_errors"].as_i64().filter(|e| *e > 0) {
            warn(format!("{e} media errors (data it couldn't read back)"));
        }
        d.wear_percent = n["percentage_used"].as_i64();
    } else if let Some(left) = life {
        d.wear_percent = Some(100 - left.clamp(0, 100));
    }
    if let Some(w) = d.wear_percent.filter(|w| *w >= 90) {
        warn(format!("{w}% of its rated write life is used"));
    }
    d.status = if failing { "failing" } else if d.problems.is_empty() { "good" } else { "warning" }.into();
    d
}

/// Every disk's health. Fails only when SMART can't be read at all.
pub fn check(policy: &Policy) -> Result<Vec<Disk>> {
    ensure!(exec::installed("smartctl"), "disk health needs smartctl (smartmontools)");
    let how = exec::elevation(policy)?;
    let mut out = Vec::new();
    for device in devices(Path::new("/sys/block")) {
        let done = exec::run_as(how.as_ref(), "smartctl", &args(&device), None, Duration::from_secs(60), 1 << 20)?;
        // smartctl's exit code is a bit mask that is nonzero for a failing
        // disk too; the JSON says what happened.
        let v: Value = serde_json::from_str(&done.stdout).unwrap_or(Value::Null);
        if v.is_null() {
            ensure!(!done.complaint().contains("Permission denied"), "reading SMART needs root: set up the privileged helper");
            continue;
        }
        let messages = v["smartctl"]["messages"].to_string();
        ensure!(!messages.contains("Permission denied"), "reading SMART needs root: set up the privileged helper");
        // Asleep (--nocheck=standby): left alone, nothing to report.
        if messages.contains("STANDBY") || messages.contains("SLEEP") {
            continue;
        }
        out.push(assess(&device, &v));
    }
    Ok(out)
}

pub struct DiskHealth;

impl Tool for DiskHealth {
    fn name(&self) -> &'static str {
        "disk_health"
    }

    fn description(&self) -> &'static str {
        "Each drive's health from its own SMART data: good, warning or failing, and why (bad or pending \
         sectors, uncorrectable errors, an SSD's wear), with its temperature and hours of use. Check it when \
         the machine is slow to read files, before trusting a drive with backups, or when system_health warns."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn call(&self, policy: &Policy, _args: Value) -> Result<Value> {
        let disks = check(policy)?;
        Ok(json!({ "disks": disks }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn judges_drives() {
        let ata = json!({
            "model_name": "WDC WD40EFRX-68N32N0", "smart_status": { "passed": true }, "temperature": { "current": 34 },
            "power_on_time": { "hours": 31877 },
            "ata_smart_attributes": { "table": [
                { "id": 5, "name": "Reallocated_Sector_Ct", "value": 200, "raw": { "value": 8 }, "when_failed": "" },
                { "id": 197, "name": "Current_Pending_Sector", "value": 200, "raw": { "value": 0 }, "when_failed": "" },
                { "id": 9, "name": "Power_On_Hours", "value": 57, "raw": { "value": 31877 }, "when_failed": "" }
            ]}
        });
        let d = assess("sda", &ata);
        assert_eq!((d.status.as_str(), d.problems.clone(), d.temperature), ("warning", vec!["8 reallocated sectors".to_string()], Some(34)));

        let dying = json!({ "smart_status": { "passed": false }, "ata_smart_attributes": { "table": [
            { "id": 5, "name": "Reallocated_Sector_Ct", "value": 1, "raw": { "value": 3021 }, "when_failed": "now" } ] } });
        assert_eq!(assess("sdb", &dying).status, "failing");

        let nvme = json!({ "model_name": "Samsung SSD 980 PRO 2TB", "smart_status": { "passed": true },
            "nvme_smart_health_information_log": { "critical_warning": 0, "available_spare": 100, "available_spare_threshold": 10,
                                                   "percentage_used": 3, "media_errors": 0 } });
        let d = assess("nvme0n1", &nvme);
        assert_eq!((d.status.as_str(), d.wear_percent), ("good", Some(3)));
        let worn = json!({ "smart_status": { "passed": true },
            "nvme_smart_health_information_log": { "critical_warning": 0, "available_spare": 4, "available_spare_threshold": 10, "percentage_used": 97 } });
        let d = assess("nvme1n1", &worn);
        assert_eq!(d.status, "failing");
        assert!(d.problems.iter().any(|p| p.contains("97%")), "{:?}", d.problems);

        // A virtual disk: no SMART at all (real smartctl 7.4 output).
        let none: Value = serde_json::from_str(r#"{"smartctl":{"messages":[{"string":"/dev/vda: Unable to detect device type","severity":"error"}],"exit_status":1}}"#).unwrap();
        let d = assess("vda", &none);
        assert_eq!((d.status.as_str(), d.problems.clone()), ("unknown", vec!["Unable to detect device type".to_string()]));

        let ssd = json!({ "smart_status": { "passed": true }, "ata_smart_attributes": { "table": [
            { "id": 231, "name": "SSD_Life_Left", "value": 6, "raw": { "value": 6 }, "when_failed": "" } ] } });
        assert_eq!(assess("sdc", &ssd).wear_percent, Some(94));
    }

    #[test]
    fn helper_takes_only_whole_disks() {
        let dir = tempfile::tempdir().unwrap();
        for n in ["sda", "nvme0n1", "loop0", "dm-0", "sr0", "zram0"] {
            std::fs::create_dir(dir.path().join(n)).unwrap();
        }
        assert_eq!(devices(dir.path()), ["nvme0n1", "sda"]);
        assert!(helper_check(&["--json=c".into(), "-a".into(), "/dev/sda".into()]).is_err());
        assert!(helper_check(&args("../etc/passwd")).is_err());
        assert!(helper_check(&["--json=c".into(), "-a".into(), "--nocheck=standby".into(), "-s".into(), "on".into(), "/dev/sda".into()]).is_err());
    }
}
