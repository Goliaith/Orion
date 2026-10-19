use std::fs;
use std::process::Command;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::policy::Policy;
use crate::tool::{parse_args, Risk, Tool};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

fn no_args_schema() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

fn read_trim(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

pub struct SystemInfo;

impl Tool for SystemInfo {
    fn name(&self) -> &'static str {
        "system_info"
    }

    fn description(&self) -> &'static str {
        "Describe this machine: hostname, OS, kernel, CPU, memory, load average and uptime."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        no_args_schema()
    }

    fn call(&self, _policy: &Policy, args: Value) -> Result<Value> {
        let NoArgs {} = parse_args(args)?;
        Ok(info())
    }
}

/// What `system_info` reports.
pub fn info() -> Value {
    {
        let os = fs::read_to_string("/etc/os-release").ok().and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                .map(|v| v.trim_matches('"').to_string())
        });
        let cpu_model = fs::read_to_string("/proc/cpuinfo").ok().and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_string())
        });
        let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let mem_kb = |key: &str| {
            meminfo
                .lines()
                .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
                .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        };
        let load: Vec<f64> = read_trim("/proc/loadavg")
            .map(|s| s.split_whitespace().take(3).filter_map(|v| v.parse().ok()).collect())
            .unwrap_or_default();
        let uptime_secs = read_trim("/proc/uptime")
            .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok());
        json!({
            "hostname": read_trim("/proc/sys/kernel/hostname"),
            "os": os,
            "kernel": read_trim("/proc/sys/kernel/osrelease"),
            "arch": std::env::consts::ARCH,
            "cpu": {
                "model": cpu_model,
                "logical_cores": std::thread::available_parallelism().map(|n| n.get()).ok(),
            },
            "memory_kb": { "total": mem_kb("MemTotal"), "available": mem_kb("MemAvailable") },
            "load_average": load,
            "uptime_secs": uptime_secs,
        })
    }
}

pub struct GpuInfo;

impl Tool for GpuInfo {
    fn name(&self) -> &'static str {
        "gpu_info"
    }

    fn description(&self) -> &'static str {
        "List GPUs. Every display/compute device comes with its vendor. For NVIDIA cards, \
         nvidia-smi also reports memory, utilization, temperature and the driver version."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        no_args_schema()
    }

    fn call(&self, _policy: &Policy, args: Value) -> Result<Value> {
        let NoArgs {} = parse_args(args)?;
        Ok(gpus())
    }
}

/// What `gpu_info` reports.
pub fn gpus() -> Value {
    json!({ "devices": drm_devices(), "nvidia": nvidia_smi() })
}

/// Cards under /sys/class/drm, identified by PCI vendor.
fn drm_devices() -> Vec<Value> {
    let Ok(entries) = fs::read_dir("/sys/class/drm") else { return Vec::new() };
    let mut cards: Vec<String> = entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("card") && n[4..].chars().all(|c| c.is_ascii_digit()))
        .collect();
    cards.sort();
    cards
        .into_iter()
        .map(|card| {
            let vendor_id = read_trim(&format!("/sys/class/drm/{card}/device/vendor"));
            let vendor = match vendor_id.as_deref() {
                Some("0x10de") => "nvidia",
                Some("0x1002") => "amd",
                Some("0x8086") => "intel",
                _ => "unknown",
            };
            json!({ "card": card, "vendor": vendor, "vendor_id": vendor_id })
        })
        .collect()
}

/// `null` when nvidia-smi is absent or fails.
fn nvidia_smi() -> Value {
    const FIELDS: [&str; 7] =
        ["index", "name", "memory.total", "memory.used", "utilization.gpu", "temperature.gpu", "driver_version"];
    let out = Command::new("nvidia-smi")
        .arg(format!("--query-gpu={}", FIELDS.join(",")))
        .arg("--format=csv,noheader,nounits")
        .output();
    let Some(out) = out.ok().filter(|o| o.status.success()) else { return Value::Null };
    let num = |s: &str| s.parse::<u64>().ok();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            let get = |i: usize| f.get(i).copied().unwrap_or("");
            json!({
                "index": num(get(0)),
                "name": get(1),
                "memory_total_mib": num(get(2)),
                "memory_used_mib": num(get(3)),
                "utilization_pct": num(get(4)),
                "temperature_c": num(get(5)),
                "driver_version": get(6),
            })
        })
        .collect()
}
