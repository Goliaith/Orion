use std::fs;

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
