//! A proof-of-concept 16550A serial device.
//!
//! Firecracker's serial subscriber is the clearest case of epoll registrations
//! standing in for program state. When the guest's receive FIFO fills up it calls
//! `ops.remove(input_fd)` so it stops being woken; a different branch adds the
//! descriptor back and has to swallow `FdAlreadyRegistered` because it cannot tell
//! which state it is in; end of file removes both descriptors and leaves behind a
//! subscriber that can never fire again.
//!
//! All three are positions in [`SerialDevice::run`].

use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow};
use logger::info_unrestricted;
use tokio::select;
use tokio::sync::Notify;
use vm_superio::serial::SerialEvents;
use vm_superio::{Serial, Trigger};
use vmm_sys_util::eventfd::{EFD_CLOEXEC, EFD_NONBLOCK, EventFd};

use crate::devices::Stop;
use crate::devices::async_fd::AsyncInput;

/// How much console input to pull from the host in one read.
const RX_CHUNK: usize = 64;

/// The UART's interrupt line.
#[derive(Debug)]
pub struct EventFdTrigger(EventFd);

impl EventFdTrigger {
    pub fn new() -> io::Result<Self> {
        Ok(Self(EventFd::new(EFD_NONBLOCK | EFD_CLOEXEC)?))
    }

    /// The descriptor registered with `KVM_IRQFD`.
    pub fn irq_fd(&self) -> &EventFd {
        &self.0
    }
}

impl Trigger for EventFdTrigger {
    type E = io::Error;

    fn trigger(&self) -> io::Result<()> {
        self.0.write(1)
    }
}

/// Bridges vm-superio's callbacks back to the device task.
#[derive(Clone, Debug)]
pub struct FifoEvents {
    drained: Arc<Notify>,
}

impl SerialEvents for FifoEvents {
    fn buffer_read(&self) {}

    fn out_byte(&self) {}

    fn tx_lost_byte(&self) {}

    /// Called on a vcpu thread once the guest has emptied the receive FIFO.
    ///
    /// Firecracker signals this through yet another eventfd
    /// (`buffer_ready_event_fd`) purely so the wakeup can reach an epoll set.
    /// Waking a task needs no descriptor, and `Notify` stores a permit, so a drain
    /// landing between the capacity check and the await is not lost.
    fn in_buffer_empty(&self) {
        self.drained.notify_one();
    }
}

type Uart<W> = Serial<EventFdTrigger, FifoEvents, W>;

/// The vcpu-thread-facing half of the serial device.
#[derive(Debug)]
pub struct SerialHandle<W: Write + Send> {
    /// The bus device the guest reads and writes synchronously.
    pub uart: Arc<Mutex<Uart<W>>>,
    /// Registered with `KVM_IRQFD`.
    pub irq_fd: EventFd,
}

/// A 16550A UART driven by its own task.
#[derive(Debug)]
pub struct SerialDevice<W: Write + Send> {
    /// Genuinely shared with vcpu threads, because guest register access is a
    /// synchronous mmio or pio exit. Note what is behind the mutex: a
    /// `vm_superio::Serial`, not the device. The host-side descriptor and the
    /// task's own state stay out of reach of other threads, so this lock is only
    /// ever held for the length of a register access and never across an await.
    uart: Arc<Mutex<Uart<W>>>,
    input: AsyncInput<OwnedFd>,
    drained: Arc<Notify>,
}

impl<W: Write + Send> SerialDevice<W> {
    /// Build a UART reading from `input` -- a pty, a pipe, or `stdin` -- and
    /// writing guest output to `output`.
    pub fn new(input: OwnedFd, output: W) -> Result<(Self, SerialHandle<W>)> {
        let irq = EventFdTrigger::new().context("create serial irqfd")?;
        let irq_fd = irq.irq_fd().try_clone().context("clone serial irqfd")?;
        let drained = Arc::new(Notify::new());
        let events = FifoEvents {
            drained: Arc::clone(&drained),
        };

        let uart = Arc::new(Mutex::new(Serial::with_events(irq, events, output)));
        let input = AsyncInput::new(input).context("register serial input")?;

        let handle = SerialHandle {
            uart: Arc::clone(&uart),
            irq_fd,
        };
        Ok((
            Self {
                uart,
                input,
                drained,
            },
            handle,
        ))
    }

    /// Feed host input to the guest until the vmm stops or the input closes.
    pub async fn run(self, mut stop: Stop) -> Result<()> {
        let mut buf = [0u8; RX_CHUNK];
        // Bytes read from the host, and how many of them the FIFO has taken. A
        // short `enqueue_raw_bytes` leaves a remainder that must survive until
        // there is room, which is why this is not just a read-then-write loop.
        let mut filled = 0;
        let mut sent = 0;

        loop {
            if sent == filled {
                sent = 0;
                filled = select! {
                    biased;
                    () = stop.requested() => return Ok(()),
                    result = self.input.read(&mut buf) => result.context("read serial input")?,
                };

                // End of file. Returning drops the descriptor with the task
                // instead of leaving an inert subscriber registered.
                if filled == 0 {
                    info_unrestricted!("serial: host input closed");
                    return Ok(());
                }
            }

            // The guest has not drained its receive FIFO yet. Not reading the
            // input is the entire back-pressure mechanism.
            while self.fifo_capacity()? == 0 {
                select! {
                    biased;
                    () = stop.requested() => return Ok(()),
                    () = self.drained.notified() => {}
                }
            }

            sent += self.enqueue(&buf[sent..filled])?;
        }
    }

    fn fifo_capacity(&self) -> Result<usize> {
        Ok(self.locked_uart()?.fifo_capacity())
    }

    fn enqueue(&self, bytes: &[u8]) -> Result<usize> {
        self.locked_uart()?
            .enqueue_raw_bytes(bytes)
            .map_err(|e| anyhow!("enqueue serial input: {e:?}"))
    }

    fn locked_uart(&self) -> Result<MutexGuard<'_, Uart<W>>> {
        self.uart
            .lock()
            .map_err(|_| anyhow!("serial uart mutex was poisoned"))
    }
}
