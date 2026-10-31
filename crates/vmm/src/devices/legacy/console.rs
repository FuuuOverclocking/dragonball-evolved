//! The stdio console: host input for the serial port, and a bounded buffer
//! that carries guest output to stdout without ever blocking a vCPU.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use rustix::termios::{self, OptionalActions, Termios};
use tokio::select;
use tokio::sync::Notify;
use tokio::time::{self, Duration, Instant};

use crate::devices::async_fd::AsyncOutput;
use crate::devices::control::{Command, Control, Flow};

pub const DEFAULT_OUTPUT_CAPACITY: usize = 64 * 1024;
/// How long a stopping drain keeps writing what is left.
const FLUSH_AT_STOP: Duration = Duration::from_secs(1);

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum ConsoleError {
    /// reopen stdin
    ReopenStdin(#[source] io::Error),
    /// inspect the reopened stdin
    StatStdin(#[source] io::Error),
    /// switch the terminal to raw mode
    RawMode(#[source] io::Error),
    /// reopen stdout
    ReopenStdout(#[source] io::Error),
    /// inspect the console output
    StatOutput(#[source] io::Error),
    /// register the console output
    RegisterOutput(#[source] io::Error),
    /// write console output
    Write(#[source] io::Error),
}

/// Host input for the console, and the terminal mode to restore afterwards.
#[derive(Debug)]
pub struct ConsoleInput {
    pub fd: OwnedFd,
    pub raw_mode: Option<RawModeGuard>,
}

/// Keeps a terminal in raw mode, restoring its original attributes on drop.
#[derive(Debug)]
pub struct RawModeGuard {
    tty: OwnedFd,
    saved: Termios,
}

impl RawModeGuard {
    pub fn new(tty: OwnedFd) -> io::Result<Self> {
        let saved = termios::tcgetattr(&tty)?;
        let mut raw = saved.clone();
        raw.make_raw();
        termios::tcsetattr(&tty, OptionalActions::Now, &raw)?;
        Ok(Self { tty, saved })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(&self.tty, OptionalActions::Now, &self.saved);
    }
}

/// Opens stdin for the console: `None` unless it is a terminal or a pipe.
///
/// Stdin is reopened through `/proc/self/fd/0`, so making it non-blocking does
/// not affect stdout and stderr when they share its open file description. A
/// terminal is switched to raw mode for as long as the returned guard lives.
pub fn open_input() -> Result<Option<ConsoleInput>, ConsoleError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open("/proc/self/fd/0")
        .map_err(ConsoleError::ReopenStdin)?;
    let is_tty = termios::isatty(&file);
    let is_fifo = file
        .metadata()
        .map_err(ConsoleError::StatStdin)?
        .file_type()
        .is_fifo();
    if !is_tty && !is_fifo {
        return Ok(None);
    }
    let raw_mode = if is_tty {
        let tty = file.try_clone().map_err(ConsoleError::RawMode)?;
        Some(RawModeGuard::new(tty.into()).map_err(ConsoleError::RawMode)?)
    } else {
        None
    };
    Ok(Some(ConsoleInput {
        fd: file.into(),
        raw_mode,
    }))
}

/// A bounded buffer of console output, written to on vCPU threads and
/// drained by [`ConsoleDrain`].
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

#[derive(Debug)]
struct Shared {
    bytes: Mutex<VecDeque<u8>>,
    ready: Notify,
    capacity: usize,
    dropped: AtomicU64,
}

impl Shared {
    fn pending(&self) -> std::sync::MutexGuard<'_, VecDeque<u8>> {
        // Nothing panics while the lock is held, so the bytes stay consistent.
        self.bytes.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The writing end. Never blocks: what does not fit is dropped and counted.
#[derive(Debug, Clone)]
pub struct ConsoleOut {
    shared: Arc<Shared>,
}

impl ConsoleOut {
    pub fn dropped_bytes(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }
}

impl Write for ConsoleOut {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut pending = self.shared.pending();
        let accepted = (self.shared.capacity - pending.len()).min(buf.len());
        pending.extend(&buf[..accepted]);
        drop(pending);
        let dropped = buf.len() - accepted;
        if dropped > 0 {
            self.shared
                .dropped
                .fetch_add(dropped as u64, Ordering::Relaxed);
        }
        if accepted > 0 {
            self.shared.ready.notify_one();
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The task that carries console output to the host.
#[derive(Debug)]
pub struct ConsoleDrain {
    shared: Arc<Shared>,
}

impl ConsoleDrain {
    /// Drains to stdout until stopped, then writes what is left for at most
    /// a second. Output is invisible to the guest, so a quiesce is answered
    /// without parking.
    pub async fn run_stdout(self, control: Control<()>) -> Result<(), ConsoleError> {
        self.run(ConsoleSink::stdout()?, control).await
    }

    async fn run(
        self,
        mut sink: ConsoleSink,
        mut control: Control<()>,
    ) -> Result<(), ConsoleError> {
        let mut flush_until = None;
        loop {
            if flush_until.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(());
            }
            let Some(bytes) = self.take() else {
                if flush_until.is_some() {
                    return Ok(());
                }
                select! {
                    biased;
                    command = control.recv() => {
                        if answer(command) == Flow::Stop {
                            flush_until = Some(Instant::now() + FLUSH_AT_STOP);
                        }
                    }
                    () = self.shared.ready.notified() => {}
                }
                continue;
            };
            let writing = sink.write_all(&bytes);
            tokio::pin!(writing);
            let written = loop {
                if let Some(deadline) = flush_until {
                    match time::timeout_at(deadline, &mut writing).await {
                        Ok(written) => break written,
                        Err(_) => return Ok(()),
                    }
                }
                select! {
                    biased;
                    command = control.recv() => {
                        if answer(command) == Flow::Stop {
                            flush_until = Some(Instant::now() + FLUSH_AT_STOP);
                        }
                    }
                    written = &mut writing => break written,
                }
            };
            written.map_err(ConsoleError::Write)?;
        }
    }

    fn take(&self) -> Option<Vec<u8>> {
        let mut pending = self.shared.pending();
        (!pending.is_empty()).then(|| pending.drain(..).collect())
    }
}

/// Answers a command without parking.
fn answer(command: Command<()>) -> Flow {
    match command {
        Command::Quiesce(ack) | Command::Resume(ack) => {
            ack.send(());
            Flow::Run
        }
        Command::Stop => Flow::Stop,
    }
}

/// Where console output goes: polled when the descriptor supports it, and
/// written on the blocking pool when it is a regular file.
#[derive(Debug)]
enum ConsoleSink {
    Async(AsyncOutput),
    Blocking(Option<File>),
}

impl ConsoleSink {
    fn stdout() -> Result<Self, ConsoleError> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open("/proc/self/fd/1")
            .map_err(ConsoleError::ReopenStdout)?;
        Self::new(file)
    }

    /// Registers `file` with the current thread's reactor if it is pollable.
    fn new(file: File) -> Result<Self, ConsoleError> {
        let file_type = file
            .metadata()
            .map_err(ConsoleError::StatOutput)?
            .file_type();
        if termios::isatty(&file) || file_type.is_fifo() || file_type.is_socket() {
            let output = AsyncOutput::new(file).map_err(ConsoleError::RegisterOutput)?;
            Ok(Self::Async(output))
        } else {
            Ok(Self::Blocking(Some(file)))
        }
    }

    async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Async(output) => output.write_all(bytes).await,
            Self::Blocking(slot) => {
                // Gone only if an earlier write was abandoned midway.
                let mut file = slot
                    .take()
                    .ok_or_else(|| io::Error::other("an abandoned write still owns the output"))?;
                let bytes = bytes.to_vec();
                let (file, written) = tokio::task::spawn_blocking(move || {
                    let written = file.write_all(&bytes);
                    (file, written)
                })
                .await
                .map_err(io::Error::other)?;
                *slot = Some(file);
                written
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::FromRawFd;

    use super::*;
    use crate::devices::async_fd::AsyncInput;
    use crate::devices::runtime::{DeviceRuntime, Response, Start};

    const TIMEOUT: Duration = Duration::from_secs(5);

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
        // SAFETY: both descriptors are fresh and owned only here.
        let (_master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        let before = termios::tcgetattr(&slave).unwrap();
        let guard = RawModeGuard::new(slave.try_clone().unwrap()).unwrap();
        let raw = termios::tcgetattr(&slave).unwrap();
        assert!(!raw.local_modes.intersects(
            termios::LocalModes::ICANON | termios::LocalModes::ECHO | termios::LocalModes::ISIG
        ));
        drop(guard);
        let after = termios::tcgetattr(&slave).unwrap();
        assert_eq!(after.input_modes, before.input_modes);
        assert_eq!(after.output_modes, before.output_modes);
        assert_eq!(after.local_modes, before.local_modes);
    }

    async fn read_exactly(input: &AsyncInput, len: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut buf = [0; 64];
        while bytes.len() < len {
            let count = time::timeout(TIMEOUT, input.read(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert_ne!(count, 0, "console output closed early");
            bytes.extend_from_slice(&buf[..count]);
        }
        bytes
    }

    #[tokio::test(flavor = "local")]
    async fn output_keeps_draining_while_quiesced_and_flushes_at_stop() {
        let (reader, writer) = io::pipe().unwrap();
        let reader = AsyncInput::new(reader).unwrap();
        let sink = File::from(OwnedFd::from(writer));
        let (mut output, drain) = output(64);
        let (mut runtime, _exits) = DeviceRuntime::new();
        let handle = runtime
            .spawn(
                "console-output",
                Start::Running,
                move |control| async move { drain.run(ConsoleSink::new(sink)?, control).await },
            )
            .unwrap();

        output.write_all(b"running ").unwrap();
        assert_eq!(read_exactly(&reader, 8).await, b"running ");

        assert_eq!(handle.quiesce(TIMEOUT).await, Response::Replied(()));
        output.write_all(b"quiesced ").unwrap();
        assert_eq!(read_exactly(&reader, 9).await, b"quiesced ");
        assert_eq!(handle.resume(TIMEOUT).await, Response::Replied(()));

        output.write_all(b"stopping").unwrap();
        handle.stop();
        time::timeout(TIMEOUT, handle.finished()).await.unwrap();
        assert_eq!(read_exactly(&reader, 8).await, b"stopping");
        assert_eq!(output.dropped_bytes(), 0);
        runtime.shutdown(TIMEOUT).await.unwrap();
    }
}
