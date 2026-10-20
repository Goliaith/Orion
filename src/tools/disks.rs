//! Disks and mounts: what is attached and how full it is, what is using
//! the space, and mounting and unmounting filesystems.
//!
//! The system's own mounts (`/`, `/boot`, `/home`, … in `[disks]
//! protected`) can't be unmounted, a disk holding them can't be mounted a
//! second time, and new mounts go only under `/mnt`, `/media` or Orion's
//! writable roots, with `nosuid,nodev`. Formatting and partitioning are
//! deliberately not offered.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::exec::{self, glob_match};
use crate::policy::Policy;
use crate::tool::{parse_args, Risk, Tool};

const LSBLK_FIELDS: &str = "NAME,PATH,TYPE,SIZE,FSTYPE,LABEL,UUID,MOUNTPOINTS,MODEL,TRAN,RM,RO,HOTPLUG,FSSIZE,FSUSED,FSAVAIL,PKNAME";

/// lsblk writes numbers and flags as JSON numbers and booleans, or (older
/// versions) as strings.
fn num(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}

fn flag(v: &Value) -> bool {
    v.as_bool().unwrap_or_else(|| matches!(v.as_str(), Some("1") | Some("true")) || v.as_u64() == Some(1))
}

fn mountpoints(v: &Value) -> Vec<String> {
    let list = match &v["mountpoints"] {
        Value::Array(a) => a.iter().filter_map(|m| m.as_str().map(String::from)).collect(),
        _ => Vec::new(),
    };
    if list.is_empty() {
        v["mountpoint"].as_str().map(|m| vec![m.to_string()]).unwrap_or_default()
    } else {
        list
    }
}

/// One block device, flattened from lsblk's tree.
#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub size: u64,
    pub fstype: Option<String>,
    pub label: Option<String>,
    pub uuid: Option<String>,
    pub mountpoints: Vec<String>,
    pub model: Option<String>,
    pub transport: Option<String>,
    pub removable: bool,
    pub read_only: bool,
    pub fs_size: Option<u64>,
    pub fs_used: Option<u64>,
    pub fs_avail: Option<u64>,
    /// The whole disk it is on (itself for a disk).
    pub disk: String,
    /// It, or something on its disk, holds a protected mount.
    pub system: bool,
}

/// Every device in `lsblk -J` output, with protected mounts marked.
pub fn parse_lsblk(text: &str, protected: &[String]) -> Result<Vec<Device>> {
    let v: Value = serde_json::from_str(text).context("lsblk's output isn't JSON")?;
    let mut out = Vec::new();
    fn walk(v: &Value, disk: &str, out: &mut Vec<Device>) {
        let s = |k: &str| v[k].as_str().filter(|x| !x.is_empty()).map(String::from);
        let path = s("path").unwrap_or_else(|| format!("/dev/{}", s("name").unwrap_or_default()));
        let kind = s("type").unwrap_or_default();
        let disk = if kind == "disk" || kind == "loop" || disk.is_empty() { path.clone() } else { disk.to_string() };
        out.push(Device {
            name: s("name").unwrap_or_default(),
            path: path.clone(),
            kind,
            size: num(&v["size"]).unwrap_or(0),
            fstype: s("fstype"),
            label: s("label"),
            uuid: s("uuid"),
            mountpoints: mountpoints(v),
            model: s("model").map(|m| m.trim().to_string()),
            transport: s("tran"),
            removable: flag(&v["rm"]) || flag(&v["hotplug"]),
            read_only: flag(&v["ro"]),
            fs_size: num(&v["fssize"]),
            fs_used: num(&v["fsused"]),
            fs_avail: num(&v["fsavail"]),
            disk: disk.clone(),
            system: false,
        });
        for c in v["children"].as_array().into_iter().flatten() {
            walk(c, &disk, out);
        }
    }
    for d in v["blockdevices"].as_array().into_iter().flatten() {
        walk(d, "", &mut out);
    }
    let system_disks: Vec<String> = out
        .iter()
        .filter(|d| d.mountpoints.iter().any(|m| is_protected(m, protected)))
        .map(|d| d.disk.clone())
        .collect();
    for d in &mut out {
        d.system = system_disks.contains(&d.disk);
    }
    Ok(out)
}

pub fn is_protected(mountpoint: &str, protected: &[String]) -> bool {
    let m = mountpoint.trim_end_matches('/');
    let m = if m.is_empty() { "/" } else { m };
    protected.iter().any(|p| glob_match(p, m))
}

fn devices(policy: &Policy) -> Result<Vec<Device>> {
    let text = exec::output("lsblk", &["-J", "-b", "-o", LSBLK_FIELDS])?;
    let mut all = parse_lsblk(&text, &policy.disks.protected)?;
    // lsblk learns filesystems from udev; without it (containers, a drive
    // udev hasn't seen yet) ask the device itself.
    for d in all.iter_mut().filter(|d| d.fstype.is_none() && d.size > 0 && d.kind != "rom") {
        if let Ok(probe) = exec::output("blkid", &["-p", "-o", "export", "--", &d.path]) {
            let field = |k: &str| probe.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix('=')).map(String::from);
            d.fstype = field("TYPE");
            d.label = d.label.take().or_else(|| field("LABEL"));
            d.uuid = d.uuid.take().or_else(|| field("UUID"));
        }
    }
    Ok(all)
}

/// A device by path, name, LABEL=, UUID= or label.
pub fn find<'a>(all: &'a [Device], spec: &str) -> Result<&'a Device> {
    let spec = spec.trim();
    let hits: Vec<&Device> = all
        .iter()
        .filter(|d| match spec.split_once('=') {
            Some(("LABEL", l)) => d.label.as_deref() == Some(l),
            Some(("UUID", u)) => d.uuid.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(u)),
            _ => d.path == spec || d.name == spec || d.label.as_deref() == Some(spec),
        })
        .collect();
    match hits.as_slice() {
        [d] => Ok(d),
        [] => bail!("there is no device {spec:?} (disk_list shows them)"),
        _ => bail!("{spec:?} matches several devices; use its path"),
    }
}

/// "512 KiB", "640 MiB", "20.7 GiB", "1.8 TiB".
fn gib(b: u64) -> String {
    let f = b as f64;
    match b {
        b if b < 1 << 20 => format!("{} KiB", b >> 10),
        b if b < 1 << 30 => format!("{:.0} MiB", f / (1u64 << 20) as f64),
        b if b < 1 << 40 => format!("{:.1} GiB", f / (1u64 << 30) as f64),
        _ => format!("{:.1} TiB", f / (1u64 << 40) as f64),
    }
}

pub struct DiskList;

impl Tool for DiskList {
    fn name(&self) -> &'static str {
        "disk_list"
    }

    fn description(&self) -> &'static str {
        "List the disks and their partitions: size, model, how they're attached (NVMe, SATA, USB), filesystem, \
         label, where each is mounted and how full it is. Marks removable drives and the disks the system runs from."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct NoArgs {}
        let NoArgs {} = parse_args(args)?;
        let all = devices(policy)?;
        let shown: Vec<Value> = all
            .iter()
            // Empty loop and RAM devices are noise.
            .filter(|d| d.size > 0 && !(d.kind == "loop" && d.mountpoints.is_empty()))
            .map(|d| {
                let mut v = json!({
                    "path": d.path, "type": d.kind, "size": gib(d.size), "model": d.model, "transport": d.transport,
                    "filesystem": d.fstype, "label": d.label, "mounted_at": d.mountpoints,
                    "removable": d.removable, "read_only": d.read_only, "system": d.system,
                });
                // "Free" is what can still be written (quotas and reserved
                // blocks make it less than size minus used).
                if let Some(avail) = d.fs_avail {
                    v["free"] = json!(gib(avail));
                }
                let used = d.fs_used.or_else(|| Some(d.fs_size?.saturating_sub(d.fs_avail?)));
                if let (Some(size), Some(used)) = (d.fs_size, used) {
                    v["used_pct"] = json!((used * 100).checked_div(size).unwrap_or(0));
                }
                v
            })
            .collect();
        Ok(json!({ "devices": shown }))
    }
}

pub struct DiskUsage;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageArgs {
    path: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    15
}

/// `du -b` lines ("bytes\tpath"), biggest first.
pub fn parse_du(text: &str, root: &Path) -> Vec<(u64, String)> {
    let mut rows: Vec<(u64, String)> = text
        .lines()
        .filter_map(|l| {
            let (n, p) = l.split_once('\t')?;
            (Path::new(p) != root).then(|| Some((n.trim().parse().ok()?, p.to_string()))).flatten()
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    rows
}

impl Tool for DiskUsage {
    fn name(&self) -> &'static str {
        "disk_usage"
    }

    fn description(&self) -> &'static str {
        "What is using the space under a folder: its biggest entries, largest first, staying on that filesystem. \
         Use it to find what is filling a disk."
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" }, "limit": { "type": "integer", "minimum": 1, "default": 15 } },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        let a: UsageArgs = parse_args(args.clone())?;
        policy.check_read(&a.path).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: UsageArgs = parse_args(args)?;
        let root = policy.check_read(&a.path)?;
        ensure!(root.is_dir(), "{} is not a folder", root.display());
        let mut cmd = std::process::Command::new("du");
        cmd.args(["-x", "-b", "--max-depth=1", "--"]).arg(&root).env("LC_ALL", "C");
        // du complains about folders it can't read and still reports the rest.
        let done = exec::run(cmd, None, Duration::from_secs(120), 4 << 20)?;
        ensure!(!done.timed_out, "du took too long; try a smaller folder");
        let total: u64 = done.stdout.lines().find(|l| l.ends_with(&format!("\t{}", root.display()))).and_then(|l| l.split('\t').next()?.parse().ok()).unwrap_or(0);
        let rows = parse_du(&done.stdout, &root);
        let fs = statvfs(&root);
        Ok(json!({
            "path": root,
            "total": gib(total),
            "filesystem_free": fs.map(|(_, free)| gib(free)),
            "filesystem_size": fs.map(|(size, _)| gib(size)),
            "biggest": rows.iter().take(a.limit).map(|(n, p)| json!({ "path": p, "size": gib(*n), "bytes": n })).collect::<Vec<_>>(),
            "unreadable": done.stderr.lines().filter(|l| l.contains("Permission denied")).count(),
        }))
    }
}

/// (size, free to unprivileged users) of the filesystem holding `p`.
fn statvfs(p: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid C string and `s` a properly sized buffer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let f = s.f_frsize;
    Some((s.f_blocks * f, s.f_bavail * f))
}

fn elevated(policy: &Policy, program: &str, args: &[String]) -> Result<exec::Finished> {
    // SAFETY: geteuid has no preconditions.
    let prefix = if unsafe { libc::geteuid() } == 0 { None } else { exec::elevation(&policy.admin.elevate)? };
    exec::run(exec::command_as(prefix.as_deref(), program, args), None, Duration::from_secs(120), 64 << 10)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MountArgs {
    device: String,
    at: Option<String>,
    #[serde(default)]
    read_only: bool,
}

/// Where a mount may go: under `/mnt`, `/media` or a writable root.
fn check_target(policy: &Policy, at: &Path) -> Result<()> {
    ensure!(at.is_absolute(), "mount at an absolute path");
    let at_s = at.display().to_string();
    ensure!(!is_protected(&at_s, &policy.disks.protected), "{at_s} is one of the system's own mount points");
    let ok_root = policy.disks.mount_roots.iter().map(|r| crate::policy::expand_home(r)).chain(policy.fs.write_roots.iter().cloned()).any(|r| at.starts_with(&r) && at != r);
    ensure!(ok_root, "mount points go under {} or a writable root", policy.disks.mount_roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", "));
    if at.exists() {
        ensure!(at.is_dir(), "{at_s} is not a folder");
        let empty = std::fs::read_dir(at).map(|mut d| d.next().is_none()).unwrap_or(false);
        ensure!(empty, "{at_s} isn't empty; mounting over it would hide what is there");
    }
    Ok(())
}

fn check_mount(policy: &Policy, a: &MountArgs) -> Result<(Device, Option<PathBuf>)> {
    let all = devices(policy)?;
    let d = find(&all, &a.device)?.clone();
    ensure!(!d.system, "{} is on the disk the system runs from; Orion doesn't mount it again", d.path);
    if let Some(m) = d.mountpoints.first() {
        bail!("{} is already mounted at {m}", d.path);
    }
    match d.fstype.as_deref() {
        None => bail!("{} has no filesystem Orion recognizes (formatting isn't something Orion does)", d.path),
        Some("swap") => bail!("{} is swap space, not a filesystem", d.path),
        Some("crypto_LUKS") => bail!("{} is encrypted; unlock it first (udisksctl unlock -b {})", d.path, d.path),
        Some(t) if t.ends_with("_member") => bail!("{} is part of a RAID or LVM set ({t}); mount the volume, not its parts", d.path),
        _ => {}
    }
    let at = a.at.as_ref().map(|p| crate::policy::expand_home(Path::new(p)));
    if let Some(at) = &at {
        check_target(policy, at)?;
    }
    Ok((d, at))
}

/// Mount options: never setuid programs or device files from a mounted
/// disk; FAT, exFAT and NTFS belong to the user, so they can write.
pub fn mount_options(fstype: &str, read_only: bool, uid: u32, gid: u32) -> String {
    let mut o = vec!["nosuid".to_string(), "nodev".to_string()];
    if read_only {
        o.push("ro".into());
    }
    if matches!(fstype, "vfat" | "exfat" | "ntfs" | "ntfs3") {
        o.push(format!("uid={uid}"));
        o.push(format!("gid={gid}"));
    }
    o.join(",")
}

pub struct DiskMount;

impl Tool for DiskMount {
    fn name(&self) -> &'static str {
        "disk_mount"
    }

    fn description(&self) -> &'static str {
        "Mount a filesystem (a USB drive, a second disk) so its files can be used: by path, LABEL= or UUID=. It \
         goes under /media or /mnt unless you say where (under /mnt, /media or a writable root), with setuid \
         programs and device files disabled."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "device": { "type": "string", "description": "/dev/sdb1, LABEL=Photos, UUID=…, or the label." },
                "at": { "type": "string", "description": "Where to mount it (optional)." },
                "read_only": { "type": "boolean", "default": false }
            },
            "required": ["device"],
            "additionalProperties": false
        })
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        check_mount(policy, &parse_args(args.clone())?).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: MountArgs = parse_args(args)?;
        let (d, at) = check_mount(policy, &a)?;
        let fstype = d.fstype.clone().unwrap_or_default();
        // SAFETY: getuid and getgid have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let options = mount_options(&fstype, a.read_only, uid, gid);
        // udisks mounts where a desktop would, and asks polkit, not sudo.
        if at.is_none() && exec::installed("udisksctl") {
            let mut argv = vec!["mount".to_string(), "-b".into(), d.path.clone(), "--no-user-interaction".into()];
            if a.read_only {
                argv.extend(["-o".into(), "ro".into()]);
            }
            let done = exec::run(exec::command_as(None, "udisksctl", &argv), None, Duration::from_secs(60), 16 << 10)?;
            if done.success() {
                return mounted(policy, &d.path, "udisks");
            }
            tracing::debug!("udisksctl: {}", done.complaint());
        }
        let name = d.label.clone().unwrap_or_else(|| d.name.clone());
        let name: String = name.chars().map(|c| if c.is_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
        let at = at.unwrap_or_else(|| Path::new("/mnt").join(&name));
        check_target(policy, &at)?;
        let mk = elevated(policy, "mkdir", &["-p".into(), "--".into(), at.display().to_string()])?;
        ensure!(mk.success(), "mkdir: {}", mk.complaint());
        let done = elevated(policy, "mount", &["-o".into(), options, "--".into(), d.path.clone(), at.display().to_string()])?;
        ensure!(done.success(), "mount: {}", done.complaint());
        mounted(policy, &d.path, "mount")
    }
}

fn mounted(policy: &Policy, path: &str, how: &str) -> Result<Value> {
    let all = devices(policy)?;
    let d = find(&all, path)?;
    Ok(json!({
        "device": d.path, "mounted_at": d.mountpoints, "filesystem": d.fstype, "label": d.label,
        "free": d.fs_avail.map(gib), "size": d.fs_size.map(gib), "by": how,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnmountArgs {
    target: String,
    #[serde(default)]
    eject: bool,
}

fn check_unmount(policy: &Policy, a: &UnmountArgs) -> Result<(Device, String)> {
    let all = devices(policy)?;
    let t = a.target.trim_end_matches('/');
    let d = match all.iter().find(|d| d.mountpoints.iter().any(|m| m.trim_end_matches('/') == t)) {
        Some(d) => d.clone(),
        None => find(&all, &a.target)?.clone(),
    };
    let at = d
        .mountpoints
        .iter()
        .find(|m| m.trim_end_matches('/') == t)
        .or_else(|| d.mountpoints.first())
        .cloned()
        .with_context(|| format!("{} isn't mounted", d.path))?;
    ensure!(!is_protected(&at, &policy.disks.protected), "{at} is one of the system's own mounts (disks.protected); no approval can unmount it");
    ensure!(!d.system || !a.eject, "{} is on the disk the system runs from; it can't be ejected", d.path);
    Ok((d, at))
}

pub struct DiskUnmount;

impl Tool for DiskUnmount {
    fn name(&self) -> &'static str {
        "disk_unmount"
    }

    fn description(&self) -> &'static str {
        "Unmount a filesystem, by where it's mounted or by device, and with eject power off a removable drive so it \
         can be unplugged safely. The system's own mounts can't be unmounted. If programs still use it, says so."
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "The mount point, or the device." },
                "eject": { "type": "boolean", "default": false, "description": "Then power off the drive (removable drives)." }
            },
            "required": ["target"],
            "additionalProperties": false
        })
    }

    fn precheck(&self, policy: &Policy, args: &Value) -> Result<()> {
        check_unmount(policy, &parse_args(args.clone())?).map(drop)
    }

    fn call(&self, policy: &Policy, args: Value) -> Result<Value> {
        let a: UnmountArgs = parse_args(args)?;
        let (d, at) = check_unmount(policy, &a)?;
        let by_udisks = exec::installed("udisksctl")
            && exec::run(
                exec::command_as(None, "udisksctl", &["unmount".into(), "-b".into(), d.path.clone(), "--no-user-interaction".into()]),
                None,
                Duration::from_secs(60),
                16 << 10,
            )
            .is_ok_and(|r| r.success());
        if !by_udisks {
            let done = elevated(policy, "umount", &["--".into(), at.clone()])?;
            if !done.success() {
                let why = done.complaint();
                if why.contains("busy") {
                    let users = elevated(policy, "fuser", &["-vm".into(), "--".into(), at.clone()]).map(|f| f.stderr).unwrap_or_default();
                    let names: Vec<&str> = users.lines().skip(1).filter_map(|l| l.split_whitespace().last()).collect();
                    bail!("{at} is in use{}; close what uses it first", if names.is_empty() { String::new() } else { format!(" by {}", names.join(", ")) });
                }
                bail!("umount: {why}");
            }
        }
        let mut ejected = false;
        if a.eject {
            ensure!(d.removable, "{} isn't a removable drive; it's unmounted, but not powered off", d.disk);
            let off = if exec::installed("udisksctl") {
                exec::run(exec::command_as(None, "udisksctl", &["power-off".into(), "-b".into(), d.disk.clone(), "--no-user-interaction".into()]), None, Duration::from_secs(60), 16 << 10)?
            } else {
                elevated(policy, "eject", &["--".into(), d.disk.clone()])?
            };
            ensure!(off.success(), "unmounted, but powering off failed: {}", off.complaint());
            ejected = true;
        }
        Ok(json!({ "unmounted": at, "device": d.path, "ejected": ejected, "safe_to_unplug": ejected }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protected() -> Vec<String> {
        crate::policy::DisksPolicy::default().protected
    }

    const LSBLK: &str = r#"{"blockdevices":[
      {"name":"nvme0n1","path":"/dev/nvme0n1","type":"disk","size":1000204886016,"fstype":null,"label":null,"uuid":null,"mountpoints":[null],"model":"Samsung SSD 990 PRO 1TB","tran":"nvme","rm":false,"ro":false,"hotplug":false,"fssize":null,"fsused":null,"fsavail":null,"pkname":null,
       "children":[
        {"name":"nvme0n1p1","path":"/dev/nvme0n1p1","type":"part","size":1073741824,"fstype":"vfat","label":null,"uuid":"A1B2-C3D4","mountpoints":["/boot/efi"],"model":null,"tran":"nvme","rm":false,"ro":false,"hotplug":false,"fssize":1071624192,"fsused":6291456,"fsavail":1065332736,"pkname":"nvme0n1"},
        {"name":"nvme0n1p2","path":"/dev/nvme0n1p2","type":"part","size":999129047040,"fstype":"ext4","label":null,"uuid":"7c1e","mountpoints":["/"],"model":null,"tran":"nvme","rm":false,"ro":false,"hotplug":false,"fssize":983349493760,"fsused":412316860416,"fsavail":521036431360,"pkname":"nvme0n1"}]},
      {"name":"sda","path":"/dev/sda","type":"disk","size":"64023257088","fstype":null,"label":null,"uuid":null,"mountpoint":null,"model":"SanDisk Ultra   ","tran":"usb","rm":"1","ro":"0","hotplug":"1",
       "children":[{"name":"sda1","path":"/dev/sda1","type":"part","size":"64022208512","fstype":"exfat","label":"PHOTOS","uuid":"1234-ABCD","mountpoint":null,"rm":"1","ro":"0","hotplug":"1"}]},
      {"name":"sdb","path":"/dev/sdb","type":"disk","size":4000787030016,"fstype":"crypto_LUKS","label":null,"uuid":"9f9f","mountpoints":[null],"tran":"sata","rm":false,"ro":false,"hotplug":false}
    ]}"#;

    #[test]
    fn reads_disks_old_and_new_lsblk() {
        let all = parse_lsblk(LSBLK, &protected()).unwrap();
        assert_eq!(all.len(), 6);
        let nvme = find(&all, "/dev/nvme0n1p2").unwrap();
        assert!(nvme.system && nvme.mountpoints == ["/"]);
        assert!(find(&all, "nvme0n1").unwrap().system, "the whole system disk is marked");
        // Older lsblk: strings for numbers and flags, `mountpoint` singular.
        let usb = find(&all, "LABEL=PHOTOS").unwrap();
        assert_eq!((usb.path.as_str(), usb.size, usb.removable, usb.system), ("/dev/sda1", 64022208512, true, false));
        assert_eq!(usb.disk, "/dev/sda");
        assert_eq!(find(&all, "PHOTOS").unwrap().path, "/dev/sda1");
        assert_eq!(find(&all, "UUID=1234-abcd").unwrap().path, "/dev/sda1");
        assert_eq!(find(&all, "sda").unwrap().model.as_deref(), Some("SanDisk Ultra"));
        assert!(find(&all, "/dev/sdz").is_err());
    }

    #[test]
    fn guards_the_system_s_own_mounts() {
        let p = protected();
        for m in ["/", "/boot", "/boot/efi", "/home", "/usr/", "/var", "/snap/core22/1380"] {
            assert!(is_protected(m, &p), "{m}");
        }
        for m in ["/mnt/photos", "/media/me/PHOTOS", "/data"] {
            assert!(!is_protected(m, &p), "{m}");
        }
        assert_eq!((gib(4096), gib(300 << 20), gib(22345330688), gib(2 << 40)), ("4 KiB".into(), "300 MiB".into(), "20.8 GiB".into(), "2.0 TiB".into()));
        assert_eq!(mount_options("exfat", false, 1000, 1000), "nosuid,nodev,uid=1000,gid=1000");
        assert_eq!(mount_options("ext4", true, 1000, 1000), "nosuid,nodev,ro");
        let du = parse_du("4096\t/home/me/a\n1048576\t/home/me/b\n1052672\t/home/me\n", Path::new("/home/me"));
        assert_eq!(du, [(1048576, "/home/me/b".into()), (4096, "/home/me/a".into())]);
    }

    /// With root and loop devices (as in this container): mount and unmount
    /// a real filesystem through the tools.
    #[test]
    fn mounts_and_unmounts_a_real_filesystem() {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 || !exec::installed("mkfs.ext4") || !exec::installed("losetup") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("disk.img");
        let ok = |cmd: &str, args: &[&str]| std::process::Command::new(cmd).args(args).output().is_ok_and(|o| o.status.success());
        assert!(ok("truncate", &["-s", "32M", img.to_str().unwrap()]));
        assert!(ok("mkfs.ext4", &["-q", "-L", "ORIONT", img.to_str().unwrap()]));
        let Ok(out) = std::process::Command::new("losetup").args(["-f", "--show", img.to_str().unwrap()]).output() else { return };
        let dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || dev.is_empty() {
            return; // no loop devices here
        }
        // Unmounted and detached however the test ends.
        struct Loop(String, PathBuf);
        impl Drop for Loop {
            fn drop(&mut self) {
                std::process::Command::new("umount").arg(&self.1).output().ok();
                std::process::Command::new("losetup").args(["-d", &self.0]).output().ok();
            }
        }
        let root = dir.path().join("mnt");
        let policy = Policy::from_toml(&format!("[disks]\nmount_roots = [{:?}]\n", root.display().to_string())).unwrap();
        let at = root.join("t");
        let _cleanup = Loop(dev.clone(), at.clone());
        let r = DiskMount.call(&policy, json!({ "device": dev, "at": at })).unwrap();
        assert_eq!(r["mounted_at"][0], at.display().to_string(), "{r}");
        assert_eq!(r["filesystem"], "ext4");
        // Mounted already; somewhere not allowed; the root filesystem.
        assert!(DiskMount.precheck(&policy, &json!({ "device": dev })).unwrap_err().to_string().contains("already mounted"));
        assert!(check_target(&policy, Path::new("/etc/x")).is_err());
        assert!(DiskUnmount.precheck(&policy, &json!({ "target": "/" })).unwrap_err().to_string().contains("system's own"));
        let u = DiskUnmount.call(&policy, json!({ "target": at })).unwrap();
        assert_eq!(u["unmounted"], at.display().to_string());
    }
}
