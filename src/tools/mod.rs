//! Built-in tools. Each module groups one area of the machine.

mod exec;
mod fs;
mod models;
mod network;
mod packages;
mod process;
mod services;
mod system;

use crate::tool::Tool;

/// Machine and GPU readings for status displays. Unlike the tools, these
/// bypass the policy and audit log, so a UI can poll them.
pub use models::loaded_models;
pub use services::failed_units;
pub use system::{gpus as gpu_status, info as machine_info};

pub fn builtin() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(system::SystemInfo),
        Box::new(system::GpuInfo),
        Box::new(network::NetInfo),
        Box::new(fs::FsList),
        Box::new(fs::FsRead),
        Box::new(fs::FsWrite),
        Box::new(process::ProcList),
        Box::new(process::ProcRun),
        Box::new(services::SvcList),
        Box::new(services::SvcStatus),
        Box::new(services::SvcControl),
        Box::new(packages::PkgList),
        Box::new(packages::PkgSearch),
        Box::new(packages::PkgInfo),
        Box::new(packages::PkgInstall),
        Box::new(packages::PkgRemove),
        Box::new(models::ModelStatus),
        Box::new(models::ModelLoad),
        Box::new(models::ModelUnload),
        Box::new(models::ModelPull),
    ]
}
