//! Awaitable file descriptors.
//!
//! Firecracker funnels every device descriptor into one process-wide epoll set
//! and then recovers the meaning of a wakeup twice over: `event-manager` maps the
//! ready descriptor back to a subscriber object, and that subscriber's
//! `process()` matches a `u32` tag packed into the high half of the epoll data
//! word to work out which of *its own* descriptors fired. Registrations can only
//! be changed from inside `process()`, which is why an inactive device has to own
//! an extra eventfd whose only purpose is to wake the device up so it can swap
//! its own registrations.
//!
//! Nothing here reproduces that. A descriptor is owned by the code that cares
//! about it and awaited at the point where its readiness is meaningful, so
//! neither dispatch step exists and there is no shared registration set to
//! mutate.

use std::io;
use std::os::fd::{AsRawFd, RawFd};

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use vmm_sys_util::eventfd::{EFD_CLOEXEC, EFD_NONBLOCK, EventFd};

/// An [`EventFd`] that can be awaited on the current thread's runtime.
///
/// Eventfds stay where the kernel writes them, notably queue notifications
/// registered through `KVM_IOEVENTFD`. Userspace-only notifications such as API
/// requests and vCPU outcomes use typed channels instead.
#[derive(Debug)]
pub struct AsyncEventFd {
    inner: AsyncFd<EventFd>,
}

impl AsyncEventFd {
    /// Create a non-blocking eventfd and register it with the current reactor.
    pub fn new() -> io::Result<Self> {
        Self::from_event_fd(EventFd::new(EFD_NONBLOCK | EFD_CLOEXEC)?)
    }

    /// Register an existing eventfd, which must have been created non-blocking.
    pub fn from_event_fd(event_fd: EventFd) -> io::Result<Self> {
        let inner = AsyncFd::with_interest(event_fd, Interest::READABLE)?;
        Ok(Self { inner })
    }

    /// A `Send` handle for kicking this descriptor from another thread.
    pub fn notifier(&self) -> io::Result<EventFd> {
        self.inner.get_ref().try_clone()
    }

    /// Wait until the counter is non-zero, then drain it and return its value.
    ///
    /// Cancel-safe, which is what makes it usable directly in a `select!` arm:
    /// the only place a notification is consumed is the synchronous `read` inside
    /// `try_io`, so dropping the future either happens before that read, leaving
    /// the counter and the reactor's readiness untouched, or after it has already
    /// returned a value.
    pub async fn read(&self) -> io::Result<u64> {
        loop {
            let mut guard = self.inner.readable().await?;
            match guard.try_io(|event_fd| event_fd.get_ref().read()) {
                Ok(count) => return count,
                // The reactor's cached readiness was stale. `try_io` has cleared
                // it, so the next `readable()` waits for a fresh wakeup instead
                // of spinning. A write racing with that clear bumps the
                // reactor's tick, which makes the clear a no-op, so a
                // notification cannot be lost here.
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsRawFd for AsyncEventFd {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.get_ref().as_raw_fd()
    }
}

/// An arbitrary readable descriptor -- a pty, a pipe, `stdin` -- that can be
/// awaited.
#[derive(Debug)]
pub struct AsyncInput<T: AsRawFd> {
    inner: AsyncFd<T>,
}

impl<T: AsRawFd> AsyncInput<T> {
    /// Switch `source` to non-blocking mode and register it with the current
    /// reactor.
    pub fn new(source: T) -> io::Result<Self> {
        set_nonblocking(source.as_raw_fd())?;
        let inner = AsyncFd::with_interest(source, Interest::READABLE)?;
        Ok(Self { inner })
    }

    /// Read the next available bytes. A return value of `0` means end of file.
    pub async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.inner.readable().await?;
            match guard.try_io(|source| read_fd(source.get_ref().as_raw_fd(), buf)) {
                Ok(count) => return count,
                Err(_would_block) => continue,
            }
        }
    }
}

fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `buf` is a valid mutable slice, so the kernel writes at most
    // `buf.len()` initialised bytes into memory this call exclusively owns.
    let count = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(count as usize)
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` is borrowed from a live `AsRawFd` for the duration of the call.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
