//! File descriptors awaited on the current thread's runtime, each owned by the
//! code that cares about its readiness.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use vmm_sys_util::eventfd::{EFD_CLOEXEC, EFD_NONBLOCK, EventFd};

/// An [`EventFd`] that can be awaited.
///
/// Eventfds stay where the kernel writes them, such as `KVM_IOEVENTFD` queue
/// notifications; notifications between threads of this process use channels.
#[derive(Debug)]
pub struct AsyncEventFd {
    inner: AsyncFd<EventFd>,
}

impl AsyncEventFd {
    /// Creates a non-blocking eventfd registered with the current reactor.
    pub fn new() -> io::Result<Self> {
        Self::from_event_fd(EventFd::new(EFD_NONBLOCK | EFD_CLOEXEC)?)
    }

    /// Registers an existing eventfd, which must be non-blocking.
    pub fn from_event_fd(event_fd: EventFd) -> io::Result<Self> {
        // SAFETY: the `AsyncFd` owns the eventfd, whose descriptor never changes.
        let inner = unsafe { AsyncFd::register_with_interest(event_fd, Interest::READABLE) }?;
        Ok(Self { inner })
    }

    /// A `Send` handle for signalling this eventfd from another thread.
    pub fn notifier(&self) -> io::Result<EventFd> {
        self.inner.get_ref().try_clone()
    }

    /// Waits until the counter is non-zero, then drains it and returns its
    /// value.
    ///
    /// Cancel-safe: a notification is consumed only by the synchronous read
    /// inside `try_io`, so dropping the future leaves it in the counter.
    pub async fn read(&self) -> io::Result<u64> {
        loop {
            let mut guard = self.inner.readable().await?;
            // `Err` means the cached readiness was stale and has been cleared.
            if let Ok(count) = guard.try_io(|event_fd| event_fd.get_ref().read()) {
                return count;
            }
        }
    }
}

impl AsRawFd for AsyncEventFd {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.get_ref().as_raw_fd()
    }
}

/// A readable descriptor, such as a pty, a pipe or stdin.
#[derive(Debug)]
pub struct AsyncInput {
    inner: AsyncFd<OwnedFd>,
}

impl AsyncInput {
    /// Switches `source` to non-blocking mode and registers it with the
    /// current reactor.
    pub fn new(source: impl Into<OwnedFd>) -> io::Result<Self> {
        Ok(Self {
            inner: register(source.into(), Interest::READABLE)?,
        })
    }

    /// Reads the next available bytes; `0` means end of file. Cancel-safe.
    pub async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.inner.readable().await?;
            if let Ok(result) = guard.try_io(|source| Ok(rustix::io::read(source, &mut *buf)?)) {
                return result;
            }
        }
    }
}

/// A writable descriptor, such as a pty, a pipe or stdout.
#[derive(Debug)]
pub struct AsyncOutput {
    inner: AsyncFd<OwnedFd>,
}

impl AsyncOutput {
    /// Switches `output` to non-blocking mode and registers it with the
    /// current reactor.
    pub fn new(output: impl Into<OwnedFd>) -> io::Result<Self> {
        Ok(Self {
            inner: register(output.into(), Interest::WRITABLE)?,
        })
    }

    /// Not cancel-safe: dropping it may leave part of `buf` written.
    pub async fn write_all(&self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let mut guard = self.inner.writable().await?;
            match guard.try_io(|output| Ok(rustix::io::write(output, buf)?)) {
                Ok(Ok(0)) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(Ok(count)) => buf = &buf[count..],
                Ok(Err(error)) => return Err(error),
                Err(_would_block) => {}
            }
        }
        Ok(())
    }
}

fn register(fd: OwnedFd, interest: Interest) -> io::Result<AsyncFd<OwnedFd>> {
    fcntl_setfl(&fd, fcntl_getfl(&fd)? | OFlags::NONBLOCK)?;
    // SAFETY: the `AsyncFd` owns `fd`, whose descriptor never changes.
    Ok(unsafe { AsyncFd::register_with_interest(fd, interest) }?)
}

#[cfg(test)]
mod tests {
    use std::os::fd::OwnedFd;

    use super::*;

    fn pipe() -> (OwnedFd, OwnedFd) {
        let (reader, writer) = io::pipe().unwrap();
        (reader.into(), writer.into())
    }

    #[tokio::test]
    async fn eventfd_reads_drain_the_counter() {
        let event = AsyncEventFd::new().unwrap();
        let notifier = event.notifier().unwrap();
        notifier.write(2).unwrap();
        notifier.write(3).unwrap();
        assert_eq!(event.read().await.unwrap(), 5);
        let pending = tokio::time::timeout(std::time::Duration::from_millis(10), event.read());
        assert!(pending.await.is_err());
        notifier.write(1).unwrap();
        assert_eq!(event.read().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn pipes_carry_bytes_and_report_end_of_file() {
        let (reader, writer) = pipe();
        let input = AsyncInput::new(reader).unwrap();
        let output = AsyncOutput::new(writer).unwrap();
        output.write_all(b"hello").await.unwrap();
        let mut buf = [0; 16];
        assert_eq!(input.read(&mut buf).await.unwrap(), 5);
        assert_eq!(&buf[..5], b"hello");
        drop(output);
        assert_eq!(input.read(&mut buf).await.unwrap(), 0);
    }
}
