//! Live readings of the machine for status displays: CPU (from
//! /proc/stat), memory, GPUs, the models loaded on local inference servers
//! and failed services. They bypass the policy and audit log, so
//! interfaces can poll them.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use serde::Serialize;

use crate::policy::Policy;

#[derive(Default, Clone, Serialize)]
pub struct Gpu {
    pub name: String,
    pub util: Option<u64>,
    pub mem_used: Option<u64>,
    pub mem_total: Option<u64>,
    pub temp: Option<u64>,
}

#[derive(Default, Clone, Serialize)]
pub struct Machine {
    pub host: String,
    pub os: String,
    pub cpu: Option<f64>,
    pub cores: Option<u64>,
    pub mem_total_kb: Option<u64>,
    pub mem_avail_kb: Option<u64>,
    pub gpus: Vec<Gpu>,
    /// Display/compute devices without nvidia-smi readings.
    pub other_gpus: Vec<String>,
    /// Models loaded on local Ollama servers: (name, VRAM MiB).
    pub loaded: Vec<(String, u64)>,
    /// Failed system services; `None` when there is no systemd.
    pub failed: Option<Vec<String>>,
}

/// CPU busy fraction between two /proc/stat samples.
fn cpu_sample() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let nums: Vec<u64> = stat.lines().next()?.split_whitespace().skip(1).filter_map(|v| v.parse().ok()).collect();
    let idle = nums.get(3)? + nums.get(4).copied().unwrap_or(0);
    Some((nums.iter().sum(), idle))
}

fn read_machine(prev_cpu: &mut Option<(u64, u64)>, with_gpu: bool, last: &Machine) -> Machine {
    let info = crate::tools::machine_info();
    let now = cpu_sample();
    let cpu = match (*prev_cpu, now) {
        (Some((t0, i0)), Some((t1, i1))) if t1 > t0 => Some(1.0 - (i1 - i0) as f64 / (t1 - t0) as f64),
        _ => None,
    };
    *prev_cpu = now;
    let mut m = Machine {
        host: info["hostname"].as_str().unwrap_or("this machine").to_string(),
        os: info["os"].as_str().unwrap_or_default().to_string(),
        cpu,
        cores: info["cpu"]["logical_cores"].as_u64(),
        mem_total_kb: info["memory_kb"]["total"].as_u64(),
        mem_avail_kb: info["memory_kb"]["available"].as_u64(),
        gpus: last.gpus.clone(),
        other_gpus: last.other_gpus.clone(),
        loaded: last.loaded.clone(),
        failed: last.failed.clone(),
    };
    if with_gpu {
        let g = crate::tools::gpu_status();
        m.gpus = g["nvidia"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|x| Gpu {
                        name: x["name"].as_str().unwrap_or("GPU").to_string(),
                        util: x["utilization_pct"].as_u64(),
                        mem_used: x["memory_used_mib"].as_u64(),
                        mem_total: x["memory_total_mib"].as_u64(),
                        temp: x["temperature_c"].as_u64(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        m.other_gpus = if m.gpus.is_empty() {
            g["devices"]
                .as_array()
                .map(|a| a.iter().filter_map(|d| d["vendor"].as_str()).filter(|v| *v != "unknown").map(str::to_string).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
    }
    m
}

/// Sample the machine every two seconds (GPUs every four, models and
/// services every eight) until `stop`.
pub fn watch_machine(policy: &Policy, tx: Sender<Machine>, stop: &AtomicBool) {
    let mut prev = cpu_sample();
    let mut last = Machine::default();
    let mut tick = 0u64;
    while !stop.load(Ordering::Relaxed) {
        last = read_machine(&mut prev, tick.is_multiple_of(2), &last);
        if tick.is_multiple_of(4) {
            last.loaded = crate::tools::loaded_models(policy);
            last.failed = crate::tools::failed_units();
        }
        if tx.send(last.clone()).is_err() {
            return;
        }
        tick += 1;
        for _ in 0..20 {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

