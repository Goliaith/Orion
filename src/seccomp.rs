//! A seccomp filter for programs the AI runs: the system calls a program
//! has no business making in Orion's sandbox fail with EPERM, everything
//! else is allowed. Refused are the calls that reach past the sandbox or
//! into the kernel's least-trodden code:
//!
//! - debugging other processes (ptrace, process_vm_readv/writev);
//! - loading kernels and modules, eBPF, perf events, userfaultfd, io_uring;
//! - mounting, pivot_root, setns, and new user namespaces (clone or
//!   unshare with CLONE_NEWUSER; clone3 answers ENOSYS so libc falls back
//!   to clone, whose flags can be checked);
//! - the kernel keyring, swap, reboot, setting the clock, raw I/O ports,
//!   open_by_handle_at (a classic container escape), process accounting.
//!
//! The filter is classic BPF, built here (no libseccomp). A system call
//! from another architecture (32-bit calls from a 64-bit program, x32)
//! kills the program. Built for x86_64 and aarch64; elsewhere there is no
//! filter.

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
use libc::sock_filter;

const LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
const JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
const JGE_K: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
const JSET_K: u16 = 0x45; // BPF_JMP | BPF_JSET | BPF_K
const RET_K: u16 = 0x06; // BPF_RET | BPF_K

const RET_ALLOW: u32 = 0x7fff_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_KILL_PROCESS: u32 = 0x8000_0000;

// struct seccomp_data: int nr; __u32 arch; __u64 ip; __u64 args[6].
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const OFF_ARG0: u32 = 16; // the low 32 bits, on these little-endian targets

const CLONE_NEWUSER: u32 = 0x1000_0000;

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

/// The calls refused outright. `debuggers`: let ptrace and
/// process_vm_readv/writev through, for developer mode's shells, whose
/// own PID namespace holds nothing else to debug.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn denied(debuggers: bool) -> Vec<libc::c_long> {
    use libc::*;
    let mut v = if debuggers { Vec::new() } else { vec![SYS_ptrace, SYS_process_vm_readv, SYS_process_vm_writev] };
    v.extend([
        SYS_kexec_load, SYS_kexec_file_load, SYS_init_module,
        SYS_finit_module, SYS_delete_module, SYS_bpf, SYS_perf_event_open, SYS_userfaultfd, SYS_io_uring_setup,
        SYS_io_uring_enter, SYS_io_uring_register, SYS_keyctl, SYS_add_key, SYS_request_key, SYS_mount, SYS_umount2,
        SYS_pivot_root, SYS_setns, SYS_fsopen, SYS_fsconfig, SYS_fsmount, SYS_fspick, SYS_move_mount, SYS_open_tree,
        SYS_mount_setattr, SYS_swapon, SYS_swapoff, SYS_reboot, SYS_settimeofday, SYS_clock_settime, SYS_clock_adjtime,
        SYS_adjtimex, SYS_acct, SYS_quotactl, SYS_open_by_handle_at, SYS_name_to_handle_at, SYS_lookup_dcookie,
    ]);
    #[cfg(target_arch = "x86_64")]
    v.extend([SYS_iopl, SYS_ioperm, SYS_uselib]);
    v
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn op(code: u16, jt: u8, jf: u8, k: u32) -> sock_filter {
    sock_filter { code, jt, jf, k }
}

/// The filter program.
pub fn program() -> Vec<libc::sock_filter> {
    program_for(false)
}

/// The filter program, with debuggers let through or not.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub fn program_for(debuggers: bool) -> Vec<sock_filter> {
    let errno = |e: i32| RET_ERRNO | (e as u32 & 0xffff);
    let mut p = vec![
        op(LD_W_ABS, 0, 0, OFF_ARCH),
        op(JEQ_K, 1, 0, AUDIT_ARCH),
        op(RET_K, 0, 0, RET_KILL_PROCESS),
        op(LD_W_ABS, 0, 0, OFF_NR),
    ];
    // x32 system calls on x86_64 carry this bit.
    #[cfg(target_arch = "x86_64")]
    p.extend([op(JGE_K, 0, 1, 0x4000_0000), op(RET_K, 0, 0, errno(libc::EPERM))]);
    for nr in denied(debuggers) {
        p.extend([op(JEQ_K, 0, 1, nr as u32), op(RET_K, 0, 0, errno(libc::EPERM))]);
    }
    p.extend([op(JEQ_K, 0, 1, libc::SYS_clone3 as u32), op(RET_K, 0, 0, errno(libc::ENOSYS))]);
    for nr in [libc::SYS_clone, libc::SYS_unshare] {
        p.extend([
            op(JEQ_K, 0, 4, nr as u32),
            op(LD_W_ABS, 0, 0, OFF_ARG0),
            op(JSET_K, 0, 1, CLONE_NEWUSER),
            op(RET_K, 0, 0, errno(libc::EPERM)),
            op(LD_W_ABS, 0, 0, OFF_NR),
        ]);
    }
    p.push(op(RET_K, 0, 0, RET_ALLOW));
    p
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn program_for(_debuggers: bool) -> Vec<libc::sock_filter> {
    Vec::new()
}

/// The program as bytes, as bwrap's `--seccomp FD` reads it.
pub fn bytes() -> Vec<u8> {
    bytes_for(false)
}

/// [`bytes`], with debuggers let through or not.
pub fn bytes_for(debuggers: bool) -> Vec<u8> {
    program_for(debuggers).iter().flat_map(|f| [f.code.to_ne_bytes().as_slice(), &[f.jt, f.jf], &f.k.to_ne_bytes()].concat()).collect()
}

/// Install the filter on the calling thread. no_new_privs must be set.
/// Only async-signal-safe calls, for use between fork and exec.
///
/// # Safety
/// `prog` must stay alive until this returns; the kernel copies it.
pub unsafe fn install(prog: &[libc::sock_filter]) -> std::io::Result<()> {
    if prog.is_empty() {
        return Ok(());
    }
    let fprog = libc::sock_fprog { len: prog.len() as u16, filter: prog.as_ptr() as *mut libc::sock_filter };
    // SAFETY: fprog points at a valid program for the duration of the call.
    if unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &fprog as *const libc::sock_fprog) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn builds_a_sound_program() {
        let p = program();
        assert!(p.len() < 4096, "BPF programs hold at most 4096 instructions");
        assert_eq!(p.last().map(|f| (f.code, f.k)), Some((RET_K, RET_ALLOW)));
        // Every jump lands inside the program.
        for (i, f) in p.iter().enumerate() {
            if f.code & 0x07 == 0x05 {
                assert!(i + 1 + (f.jt.max(f.jf) as usize) < p.len(), "jump at {i} leaves the program");
            }
        }
        assert_eq!(bytes().len(), p.len() * 8);
    }
}
