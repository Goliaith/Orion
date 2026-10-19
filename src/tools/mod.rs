//! Built-in tools. Each module groups one area of the machine.

mod fs;
mod process;
mod system;

use crate::tool::Tool;

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
