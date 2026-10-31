use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::select;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};

use crate::devices::Stop;
use crate::devices::async_fd::AsyncOutput;

pub const DEFAULT_OUTPUT_CAPACITY: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct ConsoleOut {
    shared: Arc<Shared>,
}

#[derive(Debug)]
pub struct ConsoleDrain {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    bytes: Mutex<VecDeque<u8>>,
    ready: Notify,
    capacity: usize,
    dropped: AtomicU64,
}

pub struct ConsoleInput {
    pub fd: OwnedFd,
    pub raw_mode: Option<RawModeGuard>,
}

pub struct RawModeGuard {
    file: File,
    saved: libc::termios,
}

impl RawModeGuard {
    fn new(file: File) -> io::Result<Self> {
        let mut saved = std::mem::MaybeUninit::uninit();
        // SAFETY: `file` is live and tcgetattr initializes the termios on success.
        if unsafe { libc::tcgetattr(file.as_raw_fd(), saved.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: tcgetattr succeeded.
        let saved = unsafe { saved.assume_init() };
        let mut raw = saved;
        // SAFETY: `raw` is an initialized termios.
        unsafe { libc::cfmakeraw(&mut raw) };
        // SAFETY: `file` and `raw` remain live through the call.
        if unsafe { libc::tcsetattr(file.as_raw_fd(), libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { file, saved })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // SAFETY: The guard owns the fd and the original termios.
        unsafe { libc::tcsetattr(self.file.as_raw_fd(), libc::TCSANOW, &self.saved) };
    }
}

pub fn output(capacity: usize) -> (ConsoleOut, ConsoleDrain) {
    assert!(capacity > 0, "console output capacity must be non-zero");
    let shared = Arc::new(Shared {
        bytes: Mutex::new(VecDeque::with_capacity(capacity)),
        ready: Notify::new(),
        capacity,
        dropped: AtomicU64::new(0),
    });
    (
        ConsoleOut {
            shared: Arc::clone(&shared),
        },
        ConsoleDrain { shared },
    )
}

pub fn open_input() -> Result<Option<ConsoleInput>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open("/proc/self/fd/0")
        .context("reopen stdin")?;
    let is_tty = unsafe { libc::isatty(file.as_raw_fd()) } == 1;
    let is_fifo = file
        .metadata()
        .context("stat reopened stdin")?
        .file_type()
        .is_fifo();
    if !is_tty && !is_fifo {
        return Ok(None);
    }
    let raw_mode = if is_tty {
        let guard = file.try_clone().context("clone stdin for raw-mode guard")?;
        Some(RawModeGuard::new(guard).context("set stdin raw mode")?)
    } else {
        None
    };
    Ok(Some(ConsoleInput {
        fd: file.into(),
        raw_mode,
    }))
}

impl ConsoleOut {
    pub fn dropped_bytes(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }
}

impl Write for ConsoleOut {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut pending = self
            .shared
            .bytes
            .lock()
            .map_err(|_| io::Error::other("console output mutex was poisoned"))?;
        let available = self.shared.capacity - pending.len();
        let accepted = available.min(buf.len());
        pending.extend(&buf[..accepted]);
        drop(pending);
        self.shared
            .dropped
            .fetch_add((buf.len() - accepted) as u64, Ordering::Relaxed);
        if accepted > 0 {
            self.shared.ready.notify_one();
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl ConsoleDrain {
    pub async fn run_stdout(self, mut stop: Stop) -> Result<()> {
        let mut sink = ConsoleSink::stdout()?;
        loop {
            if let Some(bytes) = self.take() {
                let writing = sink.write_all(&bytes);
                tokio::pin!(writing);
                select! {
                    result = &mut writing => result.context("write console output")?,
                    () = stop.requested() => {
                        let _ = timeout(Duration::from_secs(1), &mut writing).await;
                        return Ok(());
                    }
                }
            } else {
                select! {
                    biased;
                    () = stop.requested() => {
                        if let Some(bytes) = self.take() {
                            let _ = timeout(Duration::from_secs(1), sink.write_all(&bytes)).await;
                        }
                        return Ok(());
                    }
                    () = self.shared.ready.notified() => {}
                }
            }
        }
    }

    fn take(&self) -> Option<Vec<u8>> {
        let mut pending = self.shared.bytes.lock().ok()?;
        if pending.is_empty() {
            return None;
        }
        Some(pending.drain(..).collect())
    }
}

enum ConsoleSink {
    Async(AsyncOutput<File>),
    Blocking(Option<File>),
}

impl ConsoleSink {
    fn stdout() -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open("/proc/self/fd/1")
            .context("reopen stdout")?;
        let file_type = file.metadata().context("stat reopened stdout")?.file_type();
        let pollable = unsafe { libc::isatty(file.as_raw_fd()) } == 1
            || file_type.is_fifo()
            || file_type.is_socket();
        if pollable {
            Ok(Self::Async(
                AsyncOutput::new(file).context("register stdout")?,
            ))
        } else {
            Ok(Self::Blocking(Some(file)))
        }
    }

    async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Async(output) => output.write_all(bytes).await,
            Self::Blocking(output) => {
                let mut file = output
                    .take()
                    .expect("console writer is exclusively borrowed");
                let bytes = bytes.to_vec();
                let (file, result) = tokio::task::spawn_blocking(move || {
                    let result = Write::write_all(&mut file, &bytes);
                    (file, result)
                })
                .await
                .map_err(io::Error::other)?;
                *output = Some(file);
                result
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_output_drops_excess_without_short_writes() {
        let (mut output, drain) = output(4);
        assert_eq!(output.write(b"abcdef").unwrap(), 6);
        assert_eq!(output.dropped_bytes(), 2);
        assert_eq!(drain.take().unwrap(), b"abcd");
        assert!(drain.take().is_none());
    }

    #[test]
    fn draining_frees_capacity() {
        let (mut output, drain) = output(3);
        output.write_all(b"abc").unwrap();
        assert_eq!(drain.take().unwrap(), b"abc");
        output.write_all(b"def").unwrap();
        assert_eq!(drain.take().unwrap(), b"def");
        assert_eq!(output.dropped_bytes(), 0);
    }

    #[test]
    fn raw_mode_restores_original_terminal_attributes() {
        use std::os::fd::FromRawFd;
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: openpty initializes both descriptors on success.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        // SAFETY: Both descriptors are fresh and exclusively owned here.
        let (_master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        let attributes = || {
            let mut attributes = std::mem::MaybeUninit::uninit();
            // SAFETY: tcgetattr initializes termios on success; the fd is owned by `slave`.
            assert_eq!(
                unsafe { libc::tcgetattr(slave.as_raw_fd(), attributes.as_mut_ptr()) },
                0
            );
            // SAFETY: tcgetattr succeeded.
            unsafe { attributes.assume_init() }
        };
        let before = attributes();
        let guard = RawModeGuard::new(slave.try_clone().unwrap()).unwrap();
        assert_eq!(
            attributes().c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG),
            0
        );
        drop(guard);
        let after = attributes();
        assert_eq!(after.c_iflag, before.c_iflag);
        assert_eq!(after.c_oflag, before.c_oflag);
        assert_eq!(after.c_lflag, before.c_lflag);
    }
}
