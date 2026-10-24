//! Temperatures and fans, from the kernel's hardware monitors
//! (/sys/class/hwmon: CPU, GPU, drives, the board) and `nvidia-smi` for
//! NVIDIA's own driver, which has none. Each temperature comes with the
//! point where it gets too hot: the sensor's own critical limit where it
//! gives one, else a sensible default for its kind.

use std::path::Path;

use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};

use super::exec;
use crate::policy::Policy;
use crate::tool::{Risk, Tool};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Reading {
    /// The chip (coretemp, k10temp, amdgpu, nvme) and its label.
    pub source: String,
    pub label: String,
    /// cpu, gpu, disk, wifi or board.
    pub kind: String,
    pub celsius: f64,
    /// Too hot from here (a warning)…
    pub hot: f64,
    /// …and dangerously so from here.
    pub critical: f64,
}

fn kind(chip: &str) -> &'static str {
    match chip {
        "coretemp" | "k10temp" | "zenpower" | "cpu_thermal" | "cpu-thermal" | "soc_thermal" => "cpu",
        "amdgpu" | "radeon" | "nouveau" | "i915" | "xe" | "nvidia" => "gpu",
        "nvme" | "drivetemp" => "disk",
        c if c.starts_with("iwlwifi") || c.starts_with("ath") || c.starts_with("mt7") => "wifi",
        _ => "board",
    }
}

/// (hot, critical) for a sensor: 10 and 3 degrees under its own critical
/// limit where that is believable, else by kind.
fn limits(kind: &str, crit: Option<f64>) -> (f64, f64) {
    if let Some(c) = crit.filter(|c| (40.0..=130.0).contains(c)) {
        return (c - 10.0, c - 3.0);
    }
    match kind {
        "cpu" => (90.0, 98.0),
        "gpu" => (90.0, 100.0),
        "disk" => (70.0, 80.0),
        _ => (85.0, 95.0),
    }
}

fn millidegrees(dir: &Path, file: &str) -> Option<f64> {
    let v: f64 = std::fs::read_to_string(dir.join(file)).ok()?.trim().parse().ok()?;
    Some(v / 1000.0)
}

/// Every temperature under `root` (/sys/class/hwmon).
fn hwmon(root: &Path) -> (Vec<Reading>, Vec<Value>) {
    let (mut temps, mut fans) = (Vec::new(), Vec::new());
    let Ok(entries) = std::fs::read_dir(root) else { return (temps, fans) };
    let mut dirs: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    dirs.sort();
    for dir in dirs {
        let chip = std::fs::read_to_string(dir.join("name")).map(|s| s.trim().to_string()).unwrap_or_default();
        let k = kind(&chip);
        let mut files: Vec<String> = std::fs::read_dir(&dir).into_iter().flatten().flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
        files.sort();
        for f in &files {
            let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok().map(|s| s.trim().to_string());
            if let Some(n) = f.strip_prefix("temp").and_then(|r| r.strip_suffix("_input")) {
                let Some(c) = millidegrees(&dir, f).filter(|c| *c > -40.0 && *c < 150.0) else { continue };
                // A sensor reading 0 is one that isn't wired up.
                if c == 0.0 {
                    continue;
                }
                let crit = millidegrees(&dir, &format!("temp{n}_crit")).or_else(|| millidegrees(&dir, &format!("temp{n}_emergency")));
                let (hot, critical) = limits(k, crit);
                temps.push(Reading { source: chip.clone(), label: read(&format!("temp{n}_label")).unwrap_or_else(|| format!("temp{n}")), kind: k.into(), celsius: c, hot, critical });
            }
            if let Some(n) = f.strip_prefix("fan").and_then(|r| r.strip_suffix("_input")) {
                if let Some(rpm) = read(f).and_then(|v| v.parse::<u64>().ok()) {
                    fans.push(json!({ "source": chip, "label": read(&format!("fan{n}_label")).unwrap_or_else(|| format!("fan{n}")), "rpm": rpm }));
                }
            }
        }
    }
    (temps, fans)
}

/// NVIDIA GPUs on the proprietary driver: `index, name, temperature`.
fn parse_nvidia(text: &str) -> Vec<Reading> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            let celsius: f64 = f.get(2)?.parse().ok()?;
            let (hot, critical) = limits("gpu", None);
            Some(Reading { source: "nvidia".into(), label: format!("GPU {} ({})", f[0], f.get(1).unwrap_or(&"")), kind: "gpu".into(), celsius, hot, critical })
        })
        .collect()
}

/// Every temperature this machine reports.
pub fn temperatures() -> Vec<Reading> {
    let (mut temps, _) = hwmon(Path::new("/sys/class/hwmon"));
    if exec::installed("nvidia-smi") {
        if let Ok(t) = exec::output("nvidia-smi", &["--query-gpu=index,name,temperature.gpu", "--format=csv,noheader,nounits"]) {
            temps.extend(parse_nvidia(&t));
        }
    }
    temps
}

pub struct SensorStatus;

impl Tool for SensorStatus {
    fn name(&self) -> &'static str {
        "sensor_status"
    }

    fn description(&self) -> &'static str {
        "Temperatures (CPU, GPU, drives, the board) with the point where each gets too hot, and fan speeds. \
         Check it when the machine is loud, hot, or slows down under load (it may be throttling)."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn call(&self, _policy: &Policy, _args: Value) -> Result<Value> {
        let (_, fans) = hwmon(Path::new("/sys/class/hwmon"));
        let temps = temperatures();
        let hottest = temps.iter().map(|t| t.celsius - t.hot).fold(f64::MIN, f64::max);
        Ok(json!({
            "temperatures": temps,
            "fans": fans,
            "too_hot": temps.iter().filter(|t| t.celsius >= t.hot).map(|t| format!("{} {}", t.source, t.label)).collect::<Vec<_>>(),
            "note": temps.is_empty().then_some("no temperature sensors are visible here (a virtual machine, or the sensor drivers aren't loaded: sensors-detect)"),
            "headroom_celsius": (!temps.is_empty()).then(|| (-hottest).round()),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_sensors() {
        let dir = tempfile::tempdir().unwrap();
        let chip = |n: &str, name: &str, files: &[(&str, &str)]| {
            let d = dir.path().join(n);
            std::fs::create_dir(&d).unwrap();
            std::fs::write(d.join("name"), format!("{name}\n")).unwrap();
            for (f, v) in files {
                std::fs::write(d.join(f), format!("{v}\n")).unwrap();
            }
        };
        chip("hwmon0", "coretemp", &[("temp1_input", "93000"), ("temp1_label", "Package id 0"), ("temp1_crit", "100000"), ("temp2_input", "0")]);
        chip("hwmon1", "nvme", &[("temp1_input", "41850"), ("temp1_label", "Composite"), ("temp1_crit", "84850")]);
        chip("hwmon2", "nct6798", &[("temp1_input", "38000"), ("fan2_input", "1180"), ("fan2_label", "CPU fan")]);
        let (temps, fans) = hwmon(dir.path());
        assert_eq!(temps.len(), 3);
        assert_eq!((temps[0].kind.as_str(), temps[0].label.as_str(), temps[0].hot, temps[0].critical), ("cpu", "Package id 0", 90.0, 97.0));
        assert!(temps[0].celsius >= temps[0].hot);
        assert_eq!((temps[1].kind.as_str(), (temps[1].hot * 100.0).round() / 100.0), ("disk", 74.85));
        assert_eq!((temps[2].kind.as_str(), temps[2].hot), ("board", 85.0));
        assert_eq!(fans[0]["rpm"], 1180);
        // A believable limit only: 200 °C isn't one.
        assert_eq!(limits("cpu", Some(200.0)), (90.0, 98.0));

        let g = parse_nvidia("0, NVIDIA GeForce RTX 4090, 64\n1, NVIDIA GeForce RTX 3060, [N/A]\n");
        assert_eq!((g.len(), g[0].celsius, g[0].kind.as_str()), (1, 64.0, "gpu"));
    }
}
