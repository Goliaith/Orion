//! Built-in tools. Each module groups one area of the machine.

mod apps;
mod audio;
mod bluetooth;
mod changes;
mod clock;
mod disks;
mod display;
pub(crate) mod exec;
mod fileops;
mod flatpak;
mod fs;
pub mod gpu;
mod journal;
mod models;
mod netconf;
mod network;
mod packages;
mod power;
mod process;
mod sensors;
mod services;
pub(crate) mod session;
mod smart;
mod system;
mod timers;
mod updates;

use std::path::Path;

use anyhow::{bail, ensure, Result};

use crate::helper::{Checked, Peer};
use crate::policy::Policy;
use crate::tool::Tool;

/// Machine and GPU readings for status displays. Unlike the tools, these
/// bypass the policy and audit log, so a UI can poll them.
pub use models::loaded_models;
pub use services::failed_units;
pub use system::info as machine_info;

/// `YYYY-MM-DD HH:MM:SS` in local time.
pub fn journal_time(secs: i64) -> String {
    journal::local_time(secs)
}

/// Pending updates for the health watch: (all, security fixes, a kernel).
pub fn pending_updates(policy: &Policy) -> Result<(usize, usize, bool)> {
    updates::summary(policy)
}

/// Every disk's SMART health, for the health watch.
pub fn disk_health(policy: &Policy) -> Result<Vec<smart::Disk>> {
    smart::check(policy)
}

/// Every temperature, with its limits, for the health watch.
pub fn temperatures() -> Vec<sensors::Reading> {
    sensors::temperatures()
}

/// The batteries (power_status's view), for the health watch.
pub fn batteries() -> Vec<serde_json::Value> {
    power::batteries()
}

/// Why the machine needs a reboot, for the health watch; empty if it doesn't.
pub fn reboot_needed(policy: &Policy) -> Vec<String> {
    updates::reboot_needed(policy)
}

/// How Orion gets root for the machine tools, for `orion tools` and
/// `orion status`.
pub fn root_access(policy: &Policy) -> String {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return "Orion runs as root".into();
    }
    match exec::elevation(policy) {
        Ok(Some(exec::Elevation::Helper(socket))) => match std::os::unix::net::UnixStream::connect(&socket) {
            Ok(_) => format!("the Orion helper at {}{}", socket.display(), stale_helper_note(Path::new(HELPER_BINARY))),
            Err(e) => format!("the Orion helper at {}, which can't be reached: {e}", socket.display()),
        },
        Ok(Some(exec::Elevation::Prefix(p))) if p[0] == "sudo" => "sudo -n (needs a passwordless sudo rule)".into(),
        Ok(Some(exec::Elevation::Prefix(_))) => "pkexec (asks for a password)".into(),
        Ok(None) => "none: no helper, and elevate is none or sudo is missing".into(),
        Err(e) => format!("{e:#}"),
    }
}

/// Where the helper's unit runs Orion from (contrib/orion-helper.service).
const HELPER_BINARY: &str = "/usr/local/bin/orion";

/// A reminder when this `orion` was built after the copy the helper runs,
/// as after `cargo install` without the `sudo install` that follows it.
fn stale_helper_note(installed: &Path) -> String {
    let Ok(this) = std::env::current_exe() else { return String::new() };
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    if std::fs::canonicalize(&this).ok() == std::fs::canonicalize(installed).ok() {
        return String::new();
    }
    match (modified(&this), modified(installed)) {
        (Some(new), Some(old)) if new > old => format!(
            "\n  note: {} is older than this orion ({}), so the helper runs the old build. Update it with\n  \
             sudo install -m 755 {} {} && sudo systemctl restart orion-helper.service",
            installed.display(),
            this.display(),
            this.display(),
            installed.display(),
        ),
        _ => String::new(),
    }
}

/// The privileged helper's check of one command: the module that builds
/// that command vets it, against the helper's own policy.
pub fn helper_check(policy: &Policy, peer: &Peer, program: &str, args: &[String], stdin: Option<&str>) -> Result<Checked> {
    ensure!(stdin.is_none() || program == "tee", "{program} takes no input here");
    let plain = |r: Result<()>| r.map(|()| Checked::as_is(args, stdin));
    let drop_in = args.last().is_some_and(|p| p.starts_with("/etc/systemd/system/"));
    match program {
        "env" | "dnf" | "zypper" | "pacman" => plain(updates::helper_check(policy, program, args)),
        "flatpak" => plain(flatpak::helper_check(policy, args)),
        "snapper" => plain(changes::helper_check(policy, args)),
        "timedatectl" | "localectl" => plain(clock::helper_check(program, args)),
        "smartctl" => plain(smart::helper_check(args)),
        "systemctl" | "tee" | "rm" => plain(services::helper_check(policy, program, args, stdin)),
        "mkdir" if drop_in => plain(services::helper_check(policy, program, args, stdin)),
        "mkdir" | "mount" | "umount" | "eject" | "fuser" => plain(disks::helper_check(policy, peer.uid, peer.gid, program, args)),
        "nmcli" | "ufw" | "firewall-cmd" => netconf::helper_check(policy, peer.uid, program, args),
        other => bail!("the Orion helper doesn't run {other}"),
    }
}

pub fn builtin() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(system::SystemInfo),
        Box::new(crate::watch::SystemHealth),
        Box::new(crate::voice::tuning::VoiceCheck),
        Box::new(crate::voice::tuning::VoiceTune),
        Box::new(crate::tuning::SettingsCheck),
        Box::new(crate::tuning::SettingsTune),
        Box::new(gpu::GpuInfo),
        Box::new(network::NetInfo),
        Box::new(netconf::NetConnections),
        Box::new(netconf::WifiScan),
        Box::new(netconf::WifiConnect),
        Box::new(netconf::NetConnection),
        Box::new(netconf::FwStatus),
        Box::new(netconf::FwRule),
        Box::new(netconf::FwEnable),
        Box::new(disks::DiskList),
        Box::new(disks::DiskUsage),
        Box::new(disks::DiskMount),
        Box::new(disks::DiskUnmount),
        Box::new(fs::FsList),
        Box::new(fs::FsRead),
        Box::new(fs::FsWrite),
        Box::new(fileops::FsFind),
        Box::new(fileops::FsMkdir),
        Box::new(fileops::FsCopy),
        Box::new(fileops::FsMove),
        Box::new(fileops::FsDelete),
        Box::new(process::ProcList),
        Box::new(process::ProcRun),
        Box::new(process::ProcSignal),
        Box::new(services::SvcList),
        Box::new(services::SvcStatus),
        Box::new(services::SvcControl),
        Box::new(journal::JournalRead),
        Box::new(timers::TimerList),
        Box::new(timers::TimerCreate),
        Box::new(timers::TimerRemove),
        Box::new(packages::PkgList),
        Box::new(packages::PkgSearch),
        Box::new(packages::PkgInfo),
        Box::new(packages::PkgInstall),
        Box::new(packages::PkgRemove),
        Box::new(updates::UpdateCheck),
        Box::new(updates::UpdateApply),
        Box::new(flatpak::FlatpakSearch),
        Box::new(flatpak::FlatpakInstall),
        Box::new(flatpak::FlatpakRemove),
        Box::new(changes::PkgHistory),
        Box::new(changes::PkgRollback),
        Box::new(audio::AudioStatus),
        Box::new(audio::AudioSet),
        Box::new(bluetooth::BtStatus),
        Box::new(bluetooth::BtDevice),
        Box::new(power::PowerStatus),
        Box::new(power::PowerSet),
        Box::new(power::PowerAction),
        Box::new(display::DisplayStatus),
        Box::new(display::DisplaySet),
        Box::new(apps::AppList),
        Box::new(apps::AppOpen),
        Box::new(clock::ClockStatus),
        Box::new(clock::ClockSet),
        Box::new(smart::DiskHealth),
        Box::new(sensors::SensorStatus),
        Box::new(models::ModelStatus),
        Box::new(models::ModelLoad),
        Box::new(models::ModelUnload),
        Box::new(models::ModelPull),
        Box::new(gpu::GpuAssign),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_a_helper_left_on_an_old_build() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("orion");
        let file = std::fs::File::create(&old).unwrap();
        file.set_modified(std::time::UNIX_EPOCH).unwrap();
        let note = stale_helper_note(&old);
        assert!(note.contains("sudo install -m 755") && note.contains("restart orion-helper"), "{note}");
        assert_eq!(stale_helper_note(&std::env::current_exe().unwrap()), "", "the helper's own copy");
        assert_eq!(stale_helper_note(&dir.path().join("missing")), "");
    }
}
