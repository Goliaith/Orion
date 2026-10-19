//! Ctrl-C while the AI is working. A SIGINT handler sets a flag, and
//! anything long-running polls it: model streams, routing calls, `proc_run`
//! and approval prompts. A second Ctrl-C before the first is handled quits
//! outright, as an escape hatch for anything that does not poll.
//!
//! Blocking work that cannot poll (an HTTP request waiting for its first
//! byte) runs on a worker thread via [`wait`] or [`ChannelReader`]; on
//! interrupt the caller walks away, and the worker exits on its next read.

use std::fmt;
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

static SIGNALLED: AtomicBool = AtomicBool::new(false);
const POLL: Duration = Duration::from_millis(50);

/// The error for work stopped by the user.
#[derive(Debug, Clone, Copy)]
pub struct Interrupted;

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("interrupted by the user")
    }
}

impl std::error::Error for Interrupted {}

extern "C" fn on_sigint(_: libc::c_int) {
    if SIGNALLED.swap(true, Ordering::SeqCst) {
        // Second Ctrl-C while the first is still pending: something is not
        // listening, so leave now. _exit is async-signal-safe.
        unsafe { libc::_exit(130) };
    }
}

/// Route Ctrl-C to the flag instead of killing the process. Installed
/// without SA_RESTART, so a blocked terminal read returns and can notice.
pub fn install() {
    // SAFETY: plain sigaction with a handler that only touches an atomic
    // and calls _exit.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_sigint as extern "C" fn(libc::c_int) as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = 0;
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    }
}

/// Ask running work to stop, as Ctrl-C does. For interfaces that read
/// Ctrl-C as a key (raw mode) rather than receiving SIGINT.
pub fn request() {
    SIGNALLED.store(true, Ordering::SeqCst);
}

pub fn requested() -> bool {
    SIGNALLED.load(Ordering::SeqCst) || test_flag::get()
}

pub fn clear() {
    SIGNALLED.store(false, Ordering::SeqCst);
    test_flag::clear();
}

pub fn check() -> anyhow::Result<()> {
    if requested() {
        Err(Interrupted.into())
    } else {
        Ok(())
    }
}

pub fn is_interrupt(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Interrupted>().is_some()
        || e.downcast_ref::<io::Error>().and_then(|io| io.get_ref()).is_some_and(|inner| inner.is::<Interrupted>())
}

/// Run blocking `work` on another thread, returning early with
/// [`Interrupted`] if the user presses Ctrl-C.
pub fn wait<T: Send + 'static>(work: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(work());
    });
    loop {
        check()?;
        match rx.recv_timeout(POLL) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => anyhow::bail!("worker thread died"),
        }
    }
}

/// A reader fed by a worker thread, so reads can be interrupted. An
/// interrupted read fails with an `io::Error` wrapping [`Interrupted`]
/// (kind `Other`, so `BufRead::lines` does not retry it).
pub struct ChannelReader {
    rx: Receiver<io::Result<Vec<u8>>>,
    buf: Vec<u8>,
    at: usize,
}

impl ChannelReader {
    /// Move `inner` to a worker thread that reads it chunk by chunk. When
    /// this reader is dropped, the worker stops at its next chunk and drops
    /// `inner` (closing a connection, say).
    pub fn spawn(mut inner: impl Read + Send + 'static) -> Self {
        let (tx, rx) = mpsc::sync_channel(64);
        thread::spawn(move || {
            let mut chunk = vec![0u8; 8192];
            loop {
                let msg = match inner.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => Ok(chunk[..n].to_vec()),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => Err(e),
                };
                let failed = msg.is_err();
                if tx.send(msg).is_err() || failed {
                    break;
                }
            }
        });
        Self { rx, buf: Vec::new(), at: 0 }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.at >= self.buf.len() {
            if requested() {
                return Err(io::Error::other(Interrupted));
            }
            match self.rx.recv_timeout(POLL) {
                Ok(Ok(chunk)) => {
                    self.buf = chunk;
                    self.at = 0;
                }
                Ok(Err(e)) => return Err(e),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.at);
        out[..n].copy_from_slice(&self.buf[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// Read a line from the terminal for a y/n prompt. `None` on EOF or when
/// Ctrl-C interrupts the read (which plain `stdin().read_line` would retry).
pub fn read_answer() -> Option<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        // SAFETY: reading one byte into a valid buffer.
        let n = unsafe { libc::read(0, byte.as_mut_ptr().cast(), 1) };
        match n {
            1 if byte[0] == b'\n' => return Some(String::from_utf8_lossy(&line).into_owned()),
            1 => line.push(byte[0]),
            0 => return None,
            _ if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted && !requested() => continue,
            _ => return None,
        }
    }
}

/// In tests, a flag per test thread stands in for the process-wide one, so
/// parallel tests cannot interrupt each other.
#[cfg(test)]
pub mod test_flag {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    thread_local! {
        static FLAG: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
    }

    /// A flag that interrupts work polled on this thread once set (from any
    /// thread).
    pub fn install() -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        FLAG.with(|f| *f.borrow_mut() = Some(flag.clone()));
        flag
    }

    pub(super) fn get() -> bool {
        FLAG.with(|f| f.borrow().as_ref().is_some_and(|a| a.load(Ordering::SeqCst)))
    }

    pub(super) fn clear() {
        FLAG.with(|f| {
            if let Some(a) = f.borrow().as_ref() {
                a.store(false, Ordering::SeqCst)
            }
        });
    }
}

#[cfg(not(test))]
mod test_flag {
    pub(super) fn get() -> bool {
        false
    }
    pub(super) fn clear() {}
}

#[cfg(test)]
mod tests {
    use std::io::BufRead;
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    use super::*;

    /// A reader that never returns.
    struct Stalled;

    impl Read for Stalled {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            thread::sleep(Duration::from_secs(3600));
            Ok(0)
        }
    }

    fn interrupt_after(ms: u64) {
        let flag = test_flag::install();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(ms));
            flag.store(true, Ordering::SeqCst);
        });
    }

    #[test]
    fn stalled_reads_can_be_interrupted() {
        let data = io::Cursor::new(b"one\ntwo\n".to_vec()).chain(Stalled);
        let mut lines = io::BufReader::new(ChannelReader::spawn(data)).lines();
        assert_eq!(lines.next().unwrap().unwrap(), "one");
        assert_eq!(lines.next().unwrap().unwrap(), "two");
        interrupt_after(150);
        let start = Instant::now();
        let err = lines.next().unwrap().unwrap_err();
        assert!(is_interrupt(&anyhow::Error::new(err)));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn waits_can_be_interrupted() {
        assert_eq!(wait(|| Ok(7)).unwrap(), 7);
        interrupt_after(100);
        let err = wait(|| {
            thread::sleep(Duration::from_secs(3600));
            Ok(())
        })
        .unwrap_err();
        assert!(is_interrupt(&err));
        clear();
        assert!(!requested());
    }

    #[test]
    fn plain_eof_ends_cleanly() {
        let mut s = String::new();
        ChannelReader::spawn(io::Cursor::new(b"abc".to_vec())).read_to_string(&mut s).unwrap();
        assert_eq!(s, "abc");
    }
}
