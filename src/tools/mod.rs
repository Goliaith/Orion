//! Built-in tools. Each module groups one area of the machine.

mod fs;
mod process;
mod system;

use crate::tool::Tool;

/// Machine and GPU readings for status displays. Unlike the tools, these
/// bypass the policy and audit log, so a UI can poll them.
pub use system::{gpus as gpu_status, info as machine_info};

pub fn builtin() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(system::SystemInfo),
        Box::new(system::GpuInfo),
        Box::new(fs::FsList),
        Box::new(fs::FsRead),
        Box::new(fs::FsWrite),
        Box::new(process::ProcList),
        Box::new(process::ProcRun),
    ]
}
