//! Built-in tools. Each module groups one area of the machine.

mod changes;
mod disks;
pub(crate) mod exec;
mod fileops;
mod fs;
pub mod gpu;
mod journal;
mod models;
mod netconf;
mod network;
mod packages;
mod process;
mod services;
mod system;
mod timers;
mod updates;

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
            Ok(_) => format!("the Orion helper at {}", socket.display()),
            Err(e) => format!("the Orion helper at {}, which can't be reached: {e}", socket.display()),
        },
        Ok(Some(exec::Elevation::Prefix(p))) if p[0] == "sudo" => "sudo -n (needs a passwordless sudo rule)".into(),
        Ok(Some(exec::Elevation::Prefix(_))) => "pkexec (asks for a password)".into(),
        Ok(None) => "none: no helper, and elevate is none or sudo is missing".into(),
        Err(e) => format!("{e:#}"),
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
        "snapper" => plain(changes::helper_check(policy, args)),
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
        Box::new(changes::PkgHistory),
        Box::new(changes::PkgRollback),
        Box::new(models::ModelStatus),
        Box::new(models::ModelLoad),
        Box::new(models::ModelUnload),
        Box::new(models::ModelPull),
        Box::new(gpu::GpuAssign),
    ]
}
