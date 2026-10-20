//! GPUs: what is in the machine, what runs on each, and where models go.
//!
//! Recognition reads the kernel's view of every display and compute device
//! (sysfs under /sys/class/drm), so AMD and Intel GPUs are as visible as
//! NVIDIA ones: the real name (from the driver, nvidia-smi, or the PCI ID
//! database), memory, load, temperature, power, and the compute stacks
//! that can use it. Per-process GPU memory comes from nvidia-smi for NVIDIA
//! and from the DRM fdinfo of every other driver, and inference runners are
//! traced back to the model they serve.
//!
//! Placement predicts where a model will land (the way Ollama schedules:
//! the GPU with the most free memory that fits it whole, else split across
//! them), and `gpu_assign` pins an inference server to chosen GPUs.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::policy::Policy;
use crate::tool::{parse_args, Preapproval, Risk, Tool};

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Other,
}

impl Vendor {
    fn from_id(id: &str) -> Self {
        match id {
            "0x10de" => Vendor::Nvidia,
            "0x1002" => Vendor::Amd,
            "0x8086" => Vendor::Intel,
            _ => Vendor::Other,
        }
    }
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Its own memory.
    Discrete,
    /// Shares system memory (on the CPU die).
    Integrated,
    Unknown,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Gpu {
    /// Orion's number for it: by PCI address, so it doesn't change between
    /// boots or with the driver's own order.
    pub index: usize,
    pub name: String,
    pub vendor: Vendor,
    pub kind: Kind,
    /// PCI address, such as 0000:01:00.0.
    pub pci: String,
    pub vendor_id: String,
    pub device_id: String,
    pub driver: Option<String>,
    /// Stable identity: NVIDIA's GPU-… UUID, or AMD's unique_id as ROCm
    /// writes it.
    pub uuid: Option<String>,
    /// nvidia-smi's index, for tools that number NVIDIA cards that way.
    pub nvidia_index: Option<u32>,
    pub vram_total_mib: Option<u64>,
    pub vram_used_mib: Option<u64>,
    pub utilization_pct: Option<u64>,
    pub temperature_c: Option<u64>,
    pub power_w: Option<f64>,
    /// The card drives the boot display.
    pub primary_display: bool,
    pub render_node: Option<String>,
    /// What can compute on it here: cuda, rocm, level-zero, vulkan.
    pub compute: Vec<String>,
}

/// Where to look: the real system, or a test's fake tree.
pub struct Sources<'a> {
    pub sys_drm: &'a Path,
    pub dev: &'a Path,
    /// nvidia-smi's CSV for the GPU query, if it ran.
    pub nvidia_smi: Option<String>,
    pub pci_ids: Option<&'a str>,
    /// Vulkan driver manifests (file names).
    pub vulkan_icds: Vec<String>,
    pub level_zero: bool,
    pub rocm: bool,
}

const SMI_FIELDS: &str =
    "index,uuid,pci.bus_id,name,memory.total,memory.used,utilization.gpu,temperature.gpu,power.draw,driver_version";

fn read(p: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn link_name(p: impl AsRef<Path>) -> Option<String> {
    fs::read_link(p).ok()?.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// `0000:01:00.0`, from nvidia-smi's `00000000:01:00.0` too.
fn pci_key(addr: &str) -> String {
    let a = addr.trim().to_ascii_lowercase();
    match a.split_once(':') {
        Some((domain, rest)) if domain.len() > 4 => format!("{}:{rest}", &domain[domain.len() - 4..]),
        _ => a,
    }
}

static PCI_IDS: OnceLock<Option<String>> = OnceLock::new();

fn system_pci_ids() -> Option<&'static str> {
    PCI_IDS
        .get_or_init(|| {
            ["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids", "/usr/share/pci.ids"].iter().find_map(|p| fs::read_to_string(p).ok())
        })
        .as_deref()
}

/// The device's name in the PCI ID database: the marketing name in
/// brackets when there is one ("Navi 31 [Radeon RX 7900 XT/7900 XTX]" →
/// "Radeon RX 7900 XT/7900 XTX").
pub fn pci_name(db: &str, vendor: &str, device: &str) -> Option<String> {
    let (v, d) = (vendor.trim_start_matches("0x").to_ascii_lowercase(), device.trim_start_matches("0x").to_ascii_lowercase());
    let mut in_vendor = false;
    for line in db.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if !line.starts_with('\t') {
            if in_vendor {
                return None;
            }
            in_vendor = line.starts_with(&v) && line[v.len()..].starts_with(' ');
            continue;
        }
        if in_vendor && !line.starts_with("\t\t") {
            let l = &line[1..];
            if l.starts_with(&d) && l[d.len()..].starts_with(' ') {
                let full = l[d.len()..].trim();
                let name = match (full.find('['), full.rfind(']')) {
                    (Some(a), Some(b)) if b > a => full[a + 1..b].to_string(),
                    _ => full.to_string(),
                };
                return Some(name);
            }
        }
    }
    None
}

struct SmiRow {
    index: u32,
    uuid: String,
    pci: String,
    name: String,
    total: Option<u64>,
    used: Option<u64>,
    util: Option<u64>,
    temp: Option<u64>,
    power: Option<f64>,
}

fn parse_smi(csv: &str) -> Vec<SmiRow> {
    csv.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            if f.len() < 9 {
                return None;
            }
            let num = |s: &str| s.parse::<u64>().ok();
            Some(SmiRow {
                index: f[0].parse().ok()?,
                uuid: f[1].into(),
                pci: pci_key(f[2]),
                name: f[3].into(),
                total: num(f[4]),
                used: num(f[5]),
                util: num(f[6]),
                temp: num(f[7]),
                power: f[8].parse().ok(),
            })
        })
        .collect()
}

/// Every GPU, as seen through `src`.
pub fn scan(src: &Sources) -> Vec<Gpu> {
    let smi = src.nvidia_smi.as_deref().map(parse_smi).unwrap_or_default();
    let Ok(entries) = fs::read_dir(src.sys_drm) else { return Vec::new() };
    let mut cards: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_card = name.strip_prefix("card").is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
            let dev = e.path().join("device");
            (is_card && dev.exists()).then(|| (link_name(&dev).unwrap_or_default(), e.path()))
        })
        .collect();
    cards.sort();
    cards.dedup_by(|a, b| a.0 == b.0);
    let icd = |needle: &str| src.vulkan_icds.iter().any(|f| f.contains(needle));
    cards
        .into_iter()
        .enumerate()
        .map(|(index, (pci, card))| {
            let dev = card.join("device");
            let vendor_id = read(dev.join("vendor")).unwrap_or_default();
            let device_id = read(dev.join("device")).unwrap_or_default();
            let vendor = Vendor::from_id(&vendor_id);
            let driver = link_name(dev.join("driver"));
            let row = smi.iter().find(|r| r.pci == pci_key(&pci));
            let mib = |f: &str| read(dev.join(f)).and_then(|v| v.parse::<u64>().ok()).map(|b| b >> 20);
            let hwmon = fs::read_dir(dev.join("hwmon")).ok().and_then(|mut d| d.next()).and_then(|e| e.ok()).map(|e| e.path());
            let hw = |f: &str| hwmon.as_ref().and_then(|h| read(h.join(f))).and_then(|v| v.parse::<u64>().ok());
            let vram_total = row.and_then(|r| r.total).or_else(|| mib("mem_info_vram_total"));
            let name = row
                .map(|r| r.name.clone())
                .or_else(|| read(dev.join("product_name")))
                .or_else(|| src.pci_ids.and_then(|db| pci_name(db, &vendor_id, &device_id)))
                .unwrap_or_else(|| format!("{} GPU {}", match vendor {
                    Vendor::Nvidia => "NVIDIA",
                    Vendor::Amd => "AMD",
                    Vendor::Intel => "Intel",
                    Vendor::Other => "unknown",
                }, device_id.trim_start_matches("0x")));
            let kind = match (vendor, vram_total) {
                (Vendor::Nvidia, _) => Kind::Discrete,
                // An APU's carve-out is small; a card has gigabytes.
                (Vendor::Amd, Some(t)) => if t < 2048 { Kind::Integrated } else { Kind::Discrete },
                // Intel's iGPU sits on the root bus.
                (Vendor::Intel, _) if pci.starts_with("0000:00:") => Kind::Integrated,
                (Vendor::Intel, _) => Kind::Discrete,
                _ => Kind::Unknown,
            };
            let drv = driver.as_deref().unwrap_or_default();
            let mut compute = Vec::new();
            match drv {
                "nvidia" => compute.push("cuda".to_string()),
                "amdgpu" if src.rocm => compute.push("rocm".to_string()),
                "i915" | "xe" if src.level_zero => compute.push("level-zero".to_string()),
                _ => {}
            }
            let vk = match drv {
                "nvidia" => icd("nvidia"),
                "amdgpu" | "radeon" => icd("radeon") || icd("amd"),
                "i915" | "xe" => icd("intel"),
                _ => false,
            };
            if vk {
                compute.push("vulkan".to_string());
            }
            let render_node = fs::read_dir(dev.join("drm")).ok().and_then(|d| {
                d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).find(|n| n.starts_with("renderD"))
            });
            Gpu {
                index,
                name,
                vendor,
                kind,
                uuid: row.map(|r| r.uuid.clone()).or_else(|| read(dev.join("unique_id")).map(|u| format!("GPU-{u}"))),
                nvidia_index: row.map(|r| r.index),
                vram_total_mib: vram_total,
                vram_used_mib: row.and_then(|r| r.used).or_else(|| mib("mem_info_vram_used")),
                utilization_pct: row.and_then(|r| r.util).or_else(|| read(dev.join("gpu_busy_percent")).and_then(|v| v.parse().ok())),
                temperature_c: row.and_then(|r| r.temp).or_else(|| hw("temp1_input").map(|m| m / 1000)),
                power_w: row
                    .and_then(|r| r.power)
                    .or_else(|| hw("power1_average").or_else(|| hw("power1_input")).map(|uw| uw as f64 / 1e6)),
                primary_display: read(dev.join("boot_vga")).as_deref() == Some("1"),
                render_node: render_node.map(|n| src.dev.join("dri").join(n).display().to_string()),
                driver,
                pci,
                vendor_id,
                device_id,
                compute,
            }
        })
        .collect()
}

/// Every GPU in this machine.
pub fn inventory() -> Vec<Gpu> {
    let smi = Command::new("nvidia-smi")
        .arg(format!("--query-gpu={SMI_FIELDS}"))
        .arg("--format=csv,noheader,nounits")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    let icds: Vec<String> = ["/usr/share/vulkan/icd.d", "/etc/vulkan/icd.d"]
        .iter()
        .filter_map(|d| fs::read_dir(d).ok())
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let lib = |name: &str| ["/usr/lib/x86_64-linux-gnu", "/usr/lib64", "/usr/lib", "/usr/local/lib"].iter().any(|d| Path::new(d).join(name).exists());
    scan(&Sources {
        sys_drm: Path::new("/sys/class/drm"),
        dev: Path::new("/dev"),
        nvidia_smi: smi,
        pci_ids: system_pci_ids(),
        vulkan_icds: icds,
        level_zero: lib("libze_loader.so.1") || lib("libze_loader.so"),
        rocm: Path::new("/dev/kfd").exists() && (Path::new("/opt/rocm").exists() || lib("libamdhip64.so")),
    })
}

/// A process using GPU memory.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct GpuProcess {
    pub pid: u32,
    pub name: String,
    /// The GPU, by Orion's index.
    pub gpu: usize,
    pub vram_mib: u64,
    /// The model it serves, for inference runners Orion recognizes.
    pub model: Option<String>,
}

/// nvidia-smi's compute apps: (pid, name, MiB, PCI address).
fn parse_smi_apps(csv: &str) -> Vec<(u32, String, u64, String)> {
    csv.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            let name = f.get(1)?.rsplit('/').next().unwrap_or_default().to_string();
            Some((f.first()?.parse().ok()?, name, f.get(2)?.parse().unwrap_or(0), pci_key(f.get(3)?)))
        })
        .collect()
}

/// One DRM file's memory use, from its fdinfo: (pci, client id, MiB).
pub fn parse_fdinfo(text: &str) -> Option<(String, String, u64)> {
    let mut pdev = None;
    let mut client = None;
    let mut kib: Option<u64> = None;
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        let amount = || {
            let mut it = v.split_whitespace();
            let n: u64 = it.next()?.parse().ok()?;
            Some(match it.next() {
                Some("KiB") | Some("kB") => n,
                Some("MiB") => n << 10,
                Some("GiB") => n << 20,
                _ => n >> 10,
            })
        };
        match k.trim() {
            "drm-pdev" => pdev = Some(pci_key(v)),
            "drm-client-id" => client = Some(v.to_string()),
            // amdgpu; xe and i915 name local memory differently.
            "drm-memory-vram" | "drm-resident-vram0" | "drm-resident-local0" => kib = amount().or(kib),
            "drm-total-vram0" | "drm-total-local0" if kib.is_none() => kib = amount(),
            _ => {}
        }
    }
    Some((pdev?, client?, kib? >> 10))
}

/// What each process has on each GPU.
pub fn processes(gpus: &[Gpu]) -> Vec<GpuProcess> {
    let by_pci: HashMap<String, usize> = gpus.iter().map(|g| (pci_key(&g.pci), g.index)).collect();
    let mut out = Vec::new();
    if gpus.iter().any(|g| g.vendor == Vendor::Nvidia) {
        let apps = Command::new("nvidia-smi")
            .args(["--query-compute-apps=pid,process_name,used_memory,gpu_bus_id", "--format=csv,noheader,nounits"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| parse_smi_apps(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_default();
        for (pid, name, mib, pci) in apps {
            if let Some(&gpu) = by_pci.get(&pci) {
                out.push(GpuProcess { pid, name, gpu, vram_mib: mib, model: None });
            }
        }
    }
    // Every other driver reports per-client memory in fdinfo.
    let mut seen: HashMap<(u32, usize, String), u64> = HashMap::new();
    if let Ok(procs) = fs::read_dir("/proc") {
        for pid in procs.flatten().filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok()) {
            let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fdinfo")) else { continue };
            for fd in fds.flatten() {
                let Some(text) = read(fd.path()) else { continue };
                if !text.contains("drm-pdev") {
                    continue;
                }
                if let Some((pci, client, mib)) = parse_fdinfo(&text) {
                    if let Some(&gpu) = by_pci.get(&pci) {
                        if gpus[gpu].vendor != Vendor::Nvidia {
                            seen.insert((pid, gpu, client), mib);
                        }
                    }
                }
            }
        }
    }
    let mut sums: BTreeMap<(u32, usize), u64> = BTreeMap::new();
    for ((pid, gpu, _), mib) in seen {
        *sums.entry((pid, gpu)).or_default() += mib;
    }
    for ((pid, gpu), mib) in sums.into_iter().filter(|(_, m)| *m > 0) {
        let name = read(format!("/proc/{pid}/comm")).unwrap_or_default();
        out.push(GpuProcess { pid, name, gpu, vram_mib: mib, model: None });
    }
    for p in &mut out {
        p.model = served_model(p.pid);
    }
    out.sort_by_key(|p| (p.gpu, std::cmp::Reverse(p.vram_mib)));
    out
}

/// The model an inference process serves, from its command line: Ollama's
/// runners (traced through Ollama's manifests to the model's name),
/// llama.cpp's server, vLLM.
pub fn served_model(pid: u32) -> Option<String> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = raw.split(|b| *b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
    model_from_args(&args, ollama_name)
}

pub fn model_from_args(args: &[String], ollama: impl Fn(&Path) -> Option<String>) -> Option<String> {
    let after = |flags: &[&str]| args.windows(2).find(|w| flags.contains(&w[0].as_str())).map(|w| w[1].clone());
    let program = args.first().map(|a| a.rsplit('/').next().unwrap_or(a).to_string()).unwrap_or_default();
    let model = after(&["--model", "-m"]);
    if program.starts_with("ollama") {
        let path = PathBuf::from(model?);
        return ollama(&path).or_else(|| path.file_name().map(|f| f.to_string_lossy().into_owned()));
    }
    if args.iter().any(|a| a == "serve") && args.iter().any(|a| a.contains("vllm")) {
        return after(&["serve"]).or(model);
    }
    model.map(|m| Path::new(&m).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or(m))
}

/// Ollama stores a model's weights as blobs/sha256-…; its manifests say
/// which name a blob belongs to.
pub fn ollama_name(blob: &Path) -> Option<String> {
    let digest = blob.file_name()?.to_str()?.replacen("sha256-", "sha256:", 1);
    let manifests = blob.parent()?.parent()?.join("manifests");
    let mut stack = vec![manifests.clone()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).ok()?.flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Some(text) = read(&path) else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
            let has = v["layers"].as_array().is_some_and(|ls| ls.iter().any(|l| l["digest"] == digest.as_str()));
            if has {
                // manifests/<registry>/<namespace>/<model>/<tag>
                let rel: Vec<String> = path.strip_prefix(&manifests).ok()?.iter().map(|c| c.to_string_lossy().into_owned()).collect();
                let [.., ns, model, tag] = rel.as_slice() else { continue };
                return Some(if ns == "library" { format!("{model}:{tag}") } else { format!("{ns}/{model}:{tag}") });
            }
        }
    }
    None
}

/// One GPU's room for models.
#[derive(Clone, Debug, PartialEq)]
pub struct Budget {
    pub gpu: usize,
    pub free_mib: u64,
}

/// Room on each GPU in `allowed` (all when `None`): memory less the reserve
/// less what is in use. GPUs without memory readings are left out.
pub fn budgets(gpus: &[Gpu], reserve_mib: u64, allowed: Option<&[usize]>) -> Vec<Budget> {
    gpus.iter()
        .filter(|g| allowed.is_none_or(|a| a.contains(&g.index)))
        .filter(|g| g.kind != Kind::Integrated)
        .filter_map(|g| {
            let free = g.vram_total_mib?.saturating_sub(reserve_mib).saturating_sub(g.vram_used_mib.unwrap_or(0));
            Some(Budget { gpu: g.index, free_mib: free })
        })
        .collect()
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "placement", rename_all = "lowercase")]
pub enum Placement {
    /// Whole on one GPU.
    Single { gpu: usize },
    /// Split across these GPUs.
    Split { gpus: Vec<usize> },
    /// Part on the GPUs, the rest on the CPU (slow).
    Partial { gpus: Vec<usize>, on_gpu_pct: u64 },
    Cpu,
}

/// Where a model needing `need_mib` will go: like Ollama's scheduler, the
/// GPU with the most free memory that fits it whole; else split over all
/// that have room; else as much as fits, the rest on the CPU.
pub fn predict(need_mib: u64, budgets: &[Budget]) -> Placement {
    let mut by_free: Vec<&Budget> = budgets.iter().filter(|b| b.free_mib > 0).collect();
    by_free.sort_by(|a, b| b.free_mib.cmp(&a.free_mib).then(a.gpu.cmp(&b.gpu)));
    if let Some(b) = by_free.iter().find(|b| b.free_mib >= need_mib) {
        return Placement::Single { gpu: b.gpu };
    }
    let mut gpus: Vec<usize> = by_free.iter().map(|b| b.gpu).collect();
    gpus.sort();
    let total: u64 = by_free.iter().map(|b| b.free_mib).sum();
    match total {
        0 => Placement::Cpu,
        t if t >= need_mib => Placement::Split { gpus },
        t => Placement::Partial { gpus, on_gpu_pct: t * 100 / need_mib.max(1) },
    }
}

/// The GPUs a process may use, from the visible-devices variables in its
/// environment; `None` when it may use all (or its environment can't be
/// read).
pub fn visible_to(env: &HashMap<String, String>, gpus: &[Gpu]) -> Option<Vec<usize>> {
    let mut any = false;
    let mut out = Vec::new();
    let pick = |var: &str, vendor: Vendor, out: &mut Vec<usize>| -> bool {
        let Some(list) = env.get(var) else { return false };
        let of_vendor: Vec<&Gpu> = gpus.iter().filter(|g| g.vendor == vendor).collect();
        for item in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let found = of_vendor.iter().find(|g| g.uuid.as_deref().is_some_and(|u| u.eq_ignore_ascii_case(item)));
            let found = found.or_else(|| {
                let i: usize = item.parse().ok()?;
                // CUDA numbers by nvidia-smi's index; ROCm and others by PCI order.
                match vendor {
                    Vendor::Nvidia => of_vendor.iter().find(|g| g.nvidia_index == Some(i as u32)),
                    _ => of_vendor.get(i),
                }
            });
            if let Some(g) = found {
                out.push(g.index);
            }
        }
        true
    };
    any |= pick("CUDA_VISIBLE_DEVICES", Vendor::Nvidia, &mut out);
    any |= pick("ROCR_VISIBLE_DEVICES", Vendor::Amd, &mut out) || pick("HIP_VISIBLE_DEVICES", Vendor::Amd, &mut out);
    if !any {
        return None;
    }
    // Vendors the variables don't mention stay visible.
    let named = |v: Vendor| match v {
        Vendor::Nvidia => env.contains_key("CUDA_VISIBLE_DEVICES"),
        Vendor::Amd => env.contains_key("ROCR_VISIBLE_DEVICES") || env.contains_key("HIP_VISIBLE_DEVICES"),
        _ => false,
    };
    out.extend(gpus.iter().filter(|g| !named(g.vendor)).map(|g| g.index));
    out.sort();
    out.dedup();
    Some(out)
}

pub fn process_env(pid: u32) -> Option<HashMap<String, String>> {
    let raw = fs::read(format!("/proc/{pid}/environ")).ok()?;
    Some(
        raw.split(|b| *b == 0)
            .filter_map(|kv| {
                let s = String::from_utf8_lossy(kv);
                let (k, v) = s.split_once('=')?;
                Some((k.to_string(), v.to_string()))
            })
            .collect(),
    )
}

/// The environment that pins a program to `chosen` GPUs. NVIDIA by UUID
/// (CUDA's own numbering isn't nvidia-smi's); AMD by UUID when the card
/// has one, else by its place among AMD cards; Intel through oneAPI.
pub fn pin_env(gpus: &[Gpu], chosen: &[usize]) -> BTreeMap<&'static str, String> {
    let mut env = BTreeMap::new();
    let of = |v: Vendor| gpus.iter().filter(move |g| g.vendor == v);
    if of(Vendor::Nvidia).next().is_some() {
        let ids: Vec<String> = of(Vendor::Nvidia)
            .filter(|g| chosen.contains(&g.index))
            .map(|g| g.uuid.clone().unwrap_or_else(|| g.nvidia_index.unwrap_or(0).to_string()))
            .collect();
        // An empty list would mean "all"; -1 means none.
        env.insert("CUDA_VISIBLE_DEVICES", if ids.is_empty() { "-1".into() } else { ids.join(",") });
    }
    let amd: Vec<&Gpu> = of(Vendor::Amd).collect();
    let amd_ids: Vec<String> = amd
        .iter()
        .enumerate()
        .filter(|(_, g)| chosen.contains(&g.index))
        .map(|(i, g)| g.uuid.clone().unwrap_or_else(|| i.to_string()))
        .collect();
    if !amd_ids.is_empty() {
        env.insert("ROCR_VISIBLE_DEVICES", amd_ids.join(","));
    }
    let intel: Vec<String> = of(Vendor::Intel)
        .filter(|g| g.kind == Kind::Discrete)
        .enumerate()
        .filter(|(_, g)| chosen.contains(&g.index))
        .map(|(i, _)| format!("level_zero:{i}"))
        .collect();
    if !intel.is_empty() {
        env.insert("ONEAPI_DEVICE_SELECTOR", intel.join(";"));
    }
    env
}

/// "1", "GPU-…", a PCI address or part of a name → Orion's index.
pub fn resolve(gpus: &[Gpu], what: &str) -> Result<usize> {
    let w = what.trim();
    if let Ok(i) = w.parse::<usize>() {
        ensure!(i < gpus.len(), "there is no GPU {i} (there are {})", gpus.len());
        return Ok(i);
    }
    let lower = w.to_lowercase();
    let hits: Vec<&Gpu> = gpus
        .iter()
        .filter(|g| {
            g.uuid.as_deref().is_some_and(|u| u.eq_ignore_ascii_case(w)) || pci_key(&g.pci) == pci_key(w) || g.name.to_lowercase().contains(&lower)
        })
        .collect();
    match hits.as_slice() {
        [g] => Ok(g.index),
        [] => bail!("no GPU matches {w:?} (gpu_info lists them)"),
        _ => bail!("{w:?} matches several GPUs; use the number gpu_info gives"),
    }
}

pub struct GpuInfo;

impl Tool for GpuInfo {
    fn name(&self) -> &'static str {
        "gpu_info"
    }

    fn description(&self) -> &'static str {
        "List every GPU (NVIDIA, AMD, Intel): name, discrete or integrated, memory, load, temperature, power, the \
         compute stacks that can use it (CUDA, ROCm, oneAPI, Vulkan), and the processes on each, naming the model \
         an inference server is running."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn call(&self, _policy: &Policy, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct NoArgs {}
        let NoArgs {} = parse_args(args)?;
        let gpus = inventory();
        let procs = processes(&gpus);
        let list: Vec<Value> = gpus
            .iter()
            .map(|g| {
                let mut v = serde_json::to_value(g).unwrap_or_default();
                v["processes"] = procs.iter().filter(|p| p.gpu == g.index).map(|p| serde_json::to_value(p).unwrap_or_default()).collect();
                v
            })
            .collect();
        Ok(json!({ "gpus": list, "note": if gpus.is_empty() { Some("no GPUs found (none, or this is a container without them)") } else { None } }))
    }
}

/// `gpu_assign`: pin an inference server (a systemd unit) to chosen GPUs.
pub struct GpuAssign;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssignArgs {
    unit: String,
    /// Empty: unpin.
    gpus: Vec<Value>,
    #[serde(default)]
    user: bool,
}

/// The drop-in that pins a unit; `None` when unpinning.
pub fn drop_in(env: &BTreeMap<&'static str, String>, gpus: &[&Gpu]) -> String {
    let names: Vec<String> = gpus.iter().map(|g| format!("GPU {} ({})", g.index, g.name)).collect();
    let vars: Vec<String> = env.iter().map(|(k, v)| format!("\"{k}={v}\"")).collect();
    format!(
        "# Written by Orion (gpu_assign): {}.\n# Remove this file, or run gpu_assign with no GPUs, to undo.\n[Service]\nEnvironment={}\n",
        names.join(", "),
        vars.join(" ")
    )
}

const DROP_IN: &str = "50-orion-gpu.conf";

impl Tool for GpuAssign {
    fn name(&self) -> &'static str {
        "gpu_assign"
    }

    fn description(&self) -> &'static str {
        "Pin an inference server that runs as a systemd service (ollama, llama-server, vllm, …) to chosen GPUs, \
         and restart it: everything it loads then goes there. GPUs by gpu_info's number, UUID or name; an empty \
         list removes the pin. Use it to keep a big model on the big card, or to give two servers a GPU each."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "unit": { "type": "string", "description": "The service, such as \"ollama\"." },
                "gpus": { "type": "array", "items": { "type": ["integer", "string"] } },
                "user": { "type": "boolean", "default": false, "description": "A service of the user Orion runs as, rather than the machine's." }
            },
            "required": ["unit", "gpus"],
            "additionalProperties": false
        })
    }

    fn preapproval(&self, policy: &Policy, args: &Value) -> Preapproval {
        super::services::preapproval(policy, args)
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        let a: AssignArgs = parse_args(args.clone())?;
        super::services::check_unit(policy, &a.unit, a.user)?;
        let gpus = inventory();
        for g in &a.gpus {
            resolve(&gpus, &g.as_str().map(String::from).unwrap_or_else(|| g.to_string()))?;
        }
        Ok(())
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: AssignArgs = parse_args(args)?;
        let unit = super::services::check_unit(policy, &a.unit, a.user)?;
        let gpus = inventory();
        let mut chosen: Vec<usize> = a
            .gpus
            .iter()
            .map(|g| resolve(&gpus, &g.as_str().map(String::from).unwrap_or_else(|| g.to_string())))
            .collect::<Result<_>>()?;
        chosen.sort();
        chosen.dedup();
        let dir = if a.user {
            let home = std::env::var_os("HOME").context("HOME is not set")?;
            PathBuf::from(home).join(".config/systemd/user").join(format!("{unit}.d"))
        } else {
            PathBuf::from("/etc/systemd/system").join(format!("{unit}.d"))
        };
        let path = dir.join(DROP_IN);
        let env = pin_env(&gpus, &chosen);
        let picked: Vec<&Gpu> = gpus.iter().filter(|g| chosen.contains(&g.index)).collect();
        let content = (!chosen.is_empty()).then(|| drop_in(&env, &picked));
        super::services::write_drop_in(policy, &path, content.as_deref(), a.user)?;
        let restarted = super::services::reload_and_restart(policy, &unit, a.user)?;
        Ok(json!({
            "unit": unit,
            "gpus": picked.iter().map(|g| json!({ "index": g.index, "name": g.name })).collect::<Vec<_>>(),
            "environment": env,
            "drop_in": path,
            "pinned": content.is_some(),
            "restarted": restarted,
        }))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    /// A fake /sys/class/drm with real layouts: an RTX 4090, a Radeon RX
    /// 7900 XTX (amdgpu files and hwmon), and Intel graphics on the CPU.
    pub fn fake_sysfs(root: &Path) -> (PathBuf, String) {
        let card = |n: usize, pci: &str, vendor: &str, device: &str, driver: &str| {
            let dev = root.join("devices").join(pci);
            fs::create_dir_all(dev.join("drm/renderD12".to_string() + &n.to_string())).unwrap();
            fs::create_dir_all(root.join("drivers").join(driver)).unwrap();
            fs::write(dev.join("vendor"), vendor).unwrap();
            fs::write(dev.join("device"), device).unwrap();
            symlink(root.join("drivers").join(driver), dev.join("driver")).unwrap();
            let c = root.join("class/drm").join(format!("card{n}"));
            fs::create_dir_all(&c).unwrap();
            symlink(&dev, c.join("device")).unwrap();
            // Connector entries aren't cards.
            fs::create_dir_all(root.join("class/drm").join(format!("card{n}-DP-1"))).unwrap();
            dev
        };
        card(0, "0000:00:02.0", "0x8086", "0xa780", "i915");
        let amd = card(2, "0000:03:00.0", "0x1002", "0x744c", "amdgpu");
        fs::write(amd.join("mem_info_vram_total"), (24576u64 << 20).to_string()).unwrap();
        fs::write(amd.join("mem_info_vram_used"), (1024u64 << 20).to_string()).unwrap();
        fs::write(amd.join("gpu_busy_percent"), "37").unwrap();
        fs::write(amd.join("unique_id"), "5f3a1c2b9e8d7a60").unwrap();
        fs::create_dir_all(amd.join("hwmon/hwmon3")).unwrap();
        fs::write(amd.join("hwmon/hwmon3/temp1_input"), "54000").unwrap();
        fs::write(amd.join("hwmon/hwmon3/power1_average"), "61000000").unwrap();
        fs::write(amd.join("boot_vga"), "1").unwrap();
        card(1, "0000:01:00.0", "0x10de", "0x2684", "nvidia");
        let smi = "0, GPU-8f6c1a2e-3b4d-5e6f-7a8b-9c0d1e2f3a4b, 00000000:01:00.0, NVIDIA GeForce RTX 4090, 24564, 18200, 92, 71, 402.50, 560.35\n";
        (root.join("class/drm"), smi.to_string())
    }

    const PCI_IDS: &str = "# comment\n1002  Advanced Micro Devices, Inc. [AMD/ATI]\n\t744c  Navi 31 [Radeon RX 7900 XT/7900 XTX/7900M]\n\t\t1002 0e3b  Radeon RX 7900 XTX\n10de  NVIDIA Corporation\n\t2684  AD102 [GeForce RTX 4090]\n1425  Chelsio Communications Inc\n\t56a0  T540-50A0 Unified Wire Storage Controller\n8086  Intel Corporation\n\t56a0  DG2 [Arc A770]\n\ta780  Raptor Lake-S GT1 [UHD Graphics 770]\n";

    pub fn fake_gpus() -> Vec<Gpu> {
        let dir = tempfile::tempdir().unwrap();
        let (drm, smi) = fake_sysfs(dir.path());
        scan(&Sources {
            sys_drm: &drm,
            dev: Path::new("/dev"),
            nvidia_smi: Some(smi),
            pci_ids: Some(PCI_IDS),
            vulkan_icds: vec!["nvidia_icd.json".into(), "radeon_icd.x86_64.json".into()],
            level_zero: false,
            rocm: true,
        })
    }

    #[test]
    fn recognizes_every_gpu() {
        let g = fake_gpus();
        assert_eq!(g.len(), 3, "connector entries are not cards");
        // Numbered by PCI address, whatever the card numbers.
        let names: Vec<&str> = g.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["UHD Graphics 770", "NVIDIA GeForce RTX 4090", "Radeon RX 7900 XT/7900 XTX/7900M"]);
        let (intel, nv, amd) = (&g[0], &g[1], &g[2]);
        assert_eq!((intel.vendor, intel.kind, intel.driver.as_deref()), (Vendor::Intel, Kind::Integrated, Some("i915")));
        assert_eq!(intel.vram_total_mib, None);
        assert_eq!((nv.kind, nv.vram_total_mib, nv.vram_used_mib, nv.utilization_pct), (Kind::Discrete, Some(24564), Some(18200), Some(92)));
        assert_eq!(nv.uuid.as_deref(), Some("GPU-8f6c1a2e-3b4d-5e6f-7a8b-9c0d1e2f3a4b"));
        assert_eq!(nv.nvidia_index, Some(0));
        assert_eq!(nv.compute, ["cuda", "vulkan"]);
        assert_eq!((amd.vram_total_mib, amd.vram_used_mib, amd.utilization_pct, amd.temperature_c), (Some(24576), Some(1024), Some(37), Some(54)));
        assert_eq!(amd.power_w, Some(61.0));
        assert_eq!(amd.uuid.as_deref(), Some("GPU-5f3a1c2b9e8d7a60"));
        assert!(amd.primary_display && !nv.primary_display);
        assert_eq!(amd.compute, ["rocm", "vulkan"]);
        assert_eq!(amd.render_node.as_deref(), Some("/dev/dri/renderD122"));
    }

    #[test]
    fn names_from_the_pci_database() {
        assert_eq!(pci_name(PCI_IDS, "0x8086", "0x56a0").as_deref(), Some("Arc A770"), "within its vendor, not Chelsio's");
        assert_eq!(pci_name(PCI_IDS, "0x1425", "0x56a0").as_deref(), Some("T540-50A0 Unified Wire Storage Controller"));
        assert_eq!(pci_name(PCI_IDS, "0x10de", "0x9999"), None);
        if let Some(db) = system_pci_ids() {
            assert_eq!(pci_name(db, "0x10de", "0x2684").as_deref(), Some("GeForce RTX 4090"));
        }
    }

    #[test]
    fn reads_drm_memory_per_client() {
        let amd = "pos:\t0\nflags:\t02100002\ndrm-driver:\tamdgpu\ndrm-pdev:\t0000:03:00.0\ndrm-client-id:\t42\ndrm-memory-vram:\t5242880 KiB\ndrm-memory-gtt:\t2048 KiB\n";
        assert_eq!(parse_fdinfo(amd), Some(("0000:03:00.0".into(), "42".into(), 5120)));
        let xe = "drm-driver:\txe\ndrm-pdev:\t0000:04:00.0\ndrm-client-id:\t7\ndrm-total-vram0:\t3 GiB\ndrm-resident-vram0:\t2 GiB\n";
        assert_eq!(parse_fdinfo(xe), Some(("0000:04:00.0".into(), "7".into(), 2048)));
        assert_eq!(parse_fdinfo("pos: 0\nflags: 0\n"), None);
        let apps = parse_smi_apps("3112, /usr/local/bin/ollama, 17850, 00000000:01:00.0\n");
        assert_eq!(apps, [(3112, "ollama".into(), 17850, "0000:01:00.0".into())]);
    }

    #[test]
    fn traces_runners_to_their_model() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models");
        let manifest = models.join("manifests/registry.ollama.ai/library/qwen3/32b");
        fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        fs::write(&manifest, r#"{"layers":[{"mediaType":"application/vnd.ollama.image.model","digest":"sha256:abc123"}]}"#).unwrap();
        let blob = models.join("blobs/sha256-abc123");
        assert_eq!(ollama_name(&blob).as_deref(), Some("qwen3:32b"));
        let args = |a: &str| a.split(' ').map(String::from).collect::<Vec<_>>();
        let runner = args(&format!("/usr/local/bin/ollama runner --model {} --ctx-size 8192 --port 41231", blob.display()));
        assert_eq!(model_from_args(&runner, ollama_name).as_deref(), Some("qwen3:32b"));
        assert_eq!(model_from_args(&args("llama-server -m /models/Llama-3.1-8B-Q4_K_M.gguf --port 8080"), ollama_name).as_deref(), Some("Llama-3.1-8B-Q4_K_M.gguf"));
        assert_eq!(model_from_args(&args("/usr/bin/python3 /usr/bin/vllm serve Qwen/Qwen3-8B --port 8000"), ollama_name).as_deref(), Some("Qwen/Qwen3-8B"));
        assert_eq!(model_from_args(&args("bash"), ollama_name), None);
    }

    #[test]
    fn predicts_placement_like_ollama() {
        let b = |gpu, free_mib| Budget { gpu, free_mib };
        // The most free GPU that fits it whole.
        assert_eq!(predict(8000, &[b(1, 6000), b(2, 20000)]), Placement::Single { gpu: 2 });
        assert_eq!(predict(5000, &[b(1, 6000), b(2, 20000)]), Placement::Single { gpu: 2 });
        // Too big for any one: split.
        assert_eq!(predict(24000, &[b(1, 6000), b(2, 20000)]), Placement::Split { gpus: vec![1, 2] });
        // Too big for all: partly on the CPU.
        assert_eq!(predict(52000, &[b(1, 6000), b(2, 20000)]), Placement::Partial { gpus: vec![1, 2], on_gpu_pct: 50 });
        assert_eq!(predict(100, &[]), Placement::Cpu);

        let g = fake_gpus();
        // The integrated GPU has no budget of its own; the 4090 is mostly used.
        let all = budgets(&g, 1024, None);
        assert_eq!(all, [b(1, 24564 - 1024 - 18200), b(2, 24576 - 1024 - 1024)]);
        assert_eq!(budgets(&g, 1024, Some(&[1])), [b(1, 5340)]);
    }

    #[test]
    fn pins_and_reads_pins() {
        let g = fake_gpus();
        let env = pin_env(&g, &[2]);
        assert_eq!(env["CUDA_VISIBLE_DEVICES"], "-1", "NVIDIA hidden when not chosen");
        assert_eq!(env["ROCR_VISIBLE_DEVICES"], "GPU-5f3a1c2b9e8d7a60");
        let env = pin_env(&g, &[1]);
        assert_eq!(env["CUDA_VISIBLE_DEVICES"], "GPU-8f6c1a2e-3b4d-5e6f-7a8b-9c0d1e2f3a4b");
        assert!(!env.contains_key("ROCR_VISIBLE_DEVICES"));
        let text = drop_in(&env, &[&g[1]]);
        assert!(text.contains("[Service]\nEnvironment=\"CUDA_VISIBLE_DEVICES=GPU-8f6c1a2e-3b4d-5e6f-7a8b-9c0d1e2f3a4b\""), "{text}");

        // A server's environment says which GPUs it sees.
        let env = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>();
        assert_eq!(visible_to(&env(&[]), &g), None);
        assert_eq!(visible_to(&env(&[("CUDA_VISIBLE_DEVICES", "GPU-8f6c1a2e-3b4d-5e6f-7a8b-9c0d1e2f3a4b")]), &g), Some(vec![0, 1, 2]));
        assert_eq!(visible_to(&env(&[("CUDA_VISIBLE_DEVICES", "-1")]), &g), Some(vec![0, 2]));
        assert_eq!(visible_to(&env(&[("CUDA_VISIBLE_DEVICES", "0"), ("ROCR_VISIBLE_DEVICES", "")]), &g), Some(vec![0, 1]));

        assert_eq!(resolve(&g, "2").unwrap(), 2);
        assert_eq!(resolve(&g, "rtx 4090").unwrap(), 1);
        assert_eq!(resolve(&g, "00000000:03:00.0").unwrap(), 2);
        assert!(resolve(&g, "9").is_err() && resolve(&g, "radeon rx 7900 xt").is_ok());
    }
}
