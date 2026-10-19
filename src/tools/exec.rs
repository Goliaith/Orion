//! Running a program to completion: a timeout, capped output, Ctrl-C, and
//! no stragglers. Shared by `proc_run` and the tools that drive system
//! programs (systemctl, journalctl, package managers).

use std::io::{Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

pub struct Finished {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
    pub duration: Duration,
}

impl Finished {
    pub fn success(&self) -> bool {
        self.status.success() && !self.timed_out
    }

    pub fn signal(&self) -> Option<i32> {
        self.status.signal()
    }

    /// The program's own complaint, for error messages.
    pub fn complaint(&self) -> String {
        let text = if self.stderr.trim().is_empty() { &self.stdout } else { &self.stderr };
        let text = text.trim();
        if self.timed_out {
            format!("timed out{}", if text.is_empty() { String::new() } else { format!(": {text}") })
        } else if text.is_empty() {
            format!("exited with {}", self.status)
        } else {
            text.to_string()
        }
    }
}

/// Run `cmd` (in its own process group) until it exits or `timeout` passes,
/// keeping the first `cap` bytes of each output stream. Afterwards the whole
/// group is killed, so background children cannot linger.
pub fn run(mut cmd: Command, stdin: Option<String>, timeout: Duration, cap: usize) -> Result<Finished> {
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd.spawn().with_context(|| format!("failed to start {program:?}"))?;
    let pgid = child.id() as libc::pid_t;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        thread::spawn(move || {
            let _ = pipe.write_all(input.as_bytes());
        });
    }
    let stdout = capture(child.stdout.take(), cap);
    let stderr = capture(child.stderr.take(), cap);

    let started = Instant::now();
    let mut timed_out = false;
    let mut interrupted = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout || crate::interrupt::requested() {
            timed_out = !crate::interrupt::requested();
            interrupted = !timed_out;
            kill_group(pgid);
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(10));
    };
    // Kill leftover background children too: they would keep the output
    // pipes open and block the readers forever.
    kill_group(pgid);
    if interrupted {
        let _ = (stdout.join(), stderr.join());
        return Err(crate::interrupt::Interrupted.into());
    }
    let (stdout, stdout_truncated) = stdout.join().unwrap_or_default();
    let (stderr, stderr_truncated) = stderr.join().unwrap_or_default();
    Ok(Finished { status, stdout, stderr, stdout_truncated, stderr_truncated, timed_out, duration: started.elapsed() })
}

/// Run a quick query program and return its stdout, or its complaint.
pub fn output(program: &str, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new(program);
    cmd.args(args).env("LC_ALL", "C").env("SYSTEMD_COLORS", "0").env("SYSTEMD_PAGER", "");
    let done = run(cmd, None, Duration::from_secs(30), 4 << 20)?;
    anyhow::ensure!(done.success(), "{program}: {}", done.complaint());
    Ok(done.stdout)
}

/// How to run a program as root under `[admin] elevate`. `None` means run
/// it directly.
pub fn elevation(mode: &str) -> Result<Option<Vec<&'static str>>> {
    // SAFETY: geteuid has no preconditions.
    let root = unsafe { libc::geteuid() } == 0;
    Ok(match mode {
        "none" => None,
        "auto" if root => None,
        "auto" if installed("sudo") => Some(vec!["sudo", "-n", "--"]),
        "auto" => None,
        "sudo" => Some(vec!["sudo", "-n", "--"]),
        "pkexec" => Some(vec!["pkexec", "--"]),
        other => anyhow::bail!("admin.elevate must be auto, sudo, pkexec or none, not {other:?}"),
    })
}

/// `program args…`, behind the elevation prefix if there is one.
pub fn command_as(prefix: Option<&[&str]>, program: &str, args: &[String]) -> Command {
    let mut cmd = match prefix {
        Some([first, rest @ ..]) => {
            let mut c = Command::new(first);
            c.args(rest).arg(program);
            c
        }
        _ => Command::new(program),
    };
    cmd.args(args).env("LC_ALL", "C").env("SYSTEMD_COLORS", "0").env("SYSTEMD_PAGER", "");
    cmd
}

/// Whether `program` is on PATH.
pub fn installed(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

fn kill_group(pgid: libc::pid_t) {
    // SAFETY: kill(2) has no memory-safety preconditions; ESRCH is expected
    // when the group is already gone.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

/// Read a stream to EOF, keeping the first `cap` bytes.
fn capture(stream: Option<impl Read + Send + 'static>, cap: usize) -> JoinHandle<(String, bool)> {
    thread::spawn(move || {
        let Some(mut stream) = stream else { return (String::new(), false) };
        let mut kept = Vec::new();
        let mut truncated = false;
        let mut chunk = [0u8; 8192];
        while let Ok(n) = stream.read(&mut chunk) {
            if n == 0 {
                break;
            }
            let room = cap.saturating_sub(kept.len());
            kept.extend_from_slice(&chunk[..n.min(room)]);
            truncated |= n > room;
        }
        (String::from_utf8_lossy(&kept).into_owned(), truncated)
    })
}

/// `*` matches any run of characters; everything else matches itself.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] != '*' && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match("systemd-*", "systemd-journald.service"));
        assert!(glob_match("*", ""));
        assert!(glob_match("getty@*.service", "getty@tty1.service"));
        assert!(glob_match("ssh.service", "ssh.service"));
        assert!(!glob_match("ssh.service", "sshd.service"));
        assert!(!glob_match("linux-image-*", "linux-headers-6"));
        assert!(glob_match("*a*b", "xxaxxb"));
        assert!(!glob_match("*a*b", "xxaxxbc"));
    }

    #[test]
    fn output_reports_failures() {
        assert_eq!(output("sh", &["-c", "echo hi"]).unwrap(), "hi\n");
        let err = output("sh", &["-c", "echo nope >&2; exit 4"]).unwrap_err();
        assert_eq!(format!("{err:#}"), "sh: nope");
        assert!(installed("sh"));
        assert!(!installed("orion-no-such-program"));
    }
}
