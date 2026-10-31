//! A 16550A UART: registers on the PIO bus, host input fed by its own task.
//!
//! Back-pressure is a position in [`SerialDevice::run`]: while the guest's
//! receive FIFO is full the task does not read host input, and the guest
//! draining the FIFO wakes it.

use std::cell::Cell;
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex, MutexGuard};

use logger::{info_unlimited, warn};
use tokio::select;
use tokio::sync::Notify;
use vm_superio::serial::{Error as UartError, SerialEvents, SerialState};
use vm_superio::{Serial, Trigger};
use vmm_sys_util::eventfd::{EFD_CLOEXEC, EFD_NONBLOCK, EventFd};

use crate::bus::BusDevice;
use crate::devices::async_fd::AsyncInput;
use crate::devices::control::{Control, Flow};
use crate::space::Pio;

/// How much host input to read at once.
const RX_CHUNK: usize = 64;
/// vm-superio's receive FIFO holds this many bytes.
pub const FIFO_SIZE: usize = 64;

/// Host bytes already read but not yet accepted by the UART. No host resources
/// belong to this value. At most one read chunk may be pending.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SerialInputState {
    pub pending: Vec<u8>,
}

impl SerialInputState {
    pub const CAPACITY: usize = RX_CHUNK;

    pub fn validate(&self) -> Result<(), SerialSnapshotError> {
        if self.pending.len() > RX_CHUNK {
            return Err(SerialSnapshotError::PendingLength(self.pending.len()));
        }
        Ok(())
    }
}

/// Guest-visible UART state and the input task's unconsumed read chunk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SerialSnapshot {
    pub uart: SerialState,
    pub input: SerialInputState,
}

impl SerialSnapshot {
    /// Pure validation, before creating descriptors or registering host input.
    pub fn validate(&self) -> Result<(), SerialSnapshotError> {
        if self.uart.in_buffer.len() > FIFO_SIZE {
            return Err(SerialSnapshotError::FifoLength(self.uart.in_buffer.len()));
        }
        self.input.validate()
    }
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum SerialSnapshotError {
    /// saved serial receive FIFO length {0} exceeds its capacity
    FifoLength(usize),
    /// saved serial pending input length {0} exceeds one read chunk
    PendingLength(usize),
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum SerialError {
    /// invalid saved serial state
    Snapshot(#[from] SerialSnapshotError),
    /// restore the serial UART
    Restore(#[source] UartError<io::Error>),
    /// create or signal the serial interrupt eventfd
    Irq(#[source] io::Error),
    /// register the serial input
    RegisterInput(#[source] io::Error),
    /// read serial input
    Read(#[source] io::Error),
    /// queue serial input for the guest
    Enqueue(#[source] UartError<io::Error>),
    /// serial UART lock was poisoned
    Poisoned,
}

/// The UART's interrupt line, an eventfd for `KVM_IRQFD`.
#[derive(Debug)]
struct IrqLine {
    fd: EventFd,
    // vm-superio::Serial::from_state triggers saved interrupts. Suppress only
    // those construction-time writes; all later accesses run under the UART lock.
    armed: Cell<bool>,
}

impl Trigger for IrqLine {
    type E = io::Error;

    fn trigger(&self) -> io::Result<()> {
        if self.armed.get() {
            self.fd.write(1)?;
        }
        Ok(())
    }
}

/// Wakes the input task, from a vCPU thread, once the guest has emptied the
/// receive FIFO. `Notify` keeps a permit, so a drain between the task's
/// capacity check and its wait is not lost.
#[derive(Debug)]
struct FifoEvents {
    drained: Arc<Notify>,
}

impl SerialEvents for FifoEvents {
    fn buffer_read(&self) {}

    fn out_byte(&self) {}

    fn tx_lost_byte(&self) {}

    fn in_buffer_empty(&self) {
        self.drained.notify_one();
    }
}

type Uart<W> = Serial<IrqLine, FifoEvents, W>;

/// The registers, shared with vCPU threads. The lock is held for one register
/// access and never across an await.
#[derive(Debug)]
pub struct SerialPort<W: Write> {
    uart: Arc<Mutex<Uart<W>>>,
    irq: EventFd,
}

impl<W: Write> SerialPort<W> {
    /// The eventfd to register with `KVM_IRQFD`.
    pub fn irq_fd(&self) -> &EventFd {
        &self.irq
    }

    /// Capture registers and FIFO without acknowledging an interrupt. The
    /// caller must stop vCPUs and quiesce input before composing a snapshot.
    pub fn capture_uart(&self) -> Result<SerialState, SerialError> {
        let uart = self.uart.lock().map_err(|_| SerialError::Poisoned)?;
        Ok(uart.state())
    }

    /// Reassert the saved enabled pending interrupt without register writes or
    /// consuming input. Call after restoring CPU/LAPIC/clock state, before
    /// resuming vCPUs or input. Restoration itself never signals the IRQ fd.
    pub fn reraise_pending(&self) -> Result<(), SerialError> {
        let uart = self.uart.lock().map_err(|_| SerialError::Poisoned)?;
        let state = uart.state();
        // Match vm-superio's from_state predicates: RDA is IER bit 0 / IIR
        // bit 2, and THRE is bit 1 in both. They share one interrupt line.
        let rda = state.interrupt_enable & 1 != 0 && state.interrupt_identification & 4 != 0;
        let thre = state.interrupt_enable & 2 != 0 && state.interrupt_identification & 2 != 0;
        if rda || thre {
            uart.interrupt_evt().trigger().map_err(SerialError::Irq)?;
        }
        Ok(())
    }

    /// Bytes in the receive FIFO that the guest has not read yet.
    pub fn queued_input(&self) -> usize {
        self.uart
            .lock()
            .map_or(0, |uart| FIFO_SIZE - uart.fifo_capacity())
    }
}

impl<W: Write + Send> BusDevice<Pio> for SerialPort<W> {
    fn read(&self, offset: u16, data: &mut [u8]) {
        let ([byte], Ok(offset)) = (data, u8::try_from(offset)) else {
            return;
        };
        match self.uart.lock() {
            Ok(mut uart) => *byte = uart.read(offset),
            Err(_) => warn!("serial UART lock was poisoned"),
        }
    }

    fn write(&self, offset: u16, data: &[u8]) {
        let (&[value], Ok(offset)) = (data, u8::try_from(offset)) else {
            return;
        };
        match self.uart.lock() {
            Ok(mut uart) => {
                if let Err(error) = uart.write(offset, value) {
                    warn!("serial register write failed: {error}");
                }
            }
            Err(_) => warn!("serial UART lock was poisoned"),
        }
    }
}

/// The input task's state.
#[derive(Debug)]
pub struct SerialDevice<W: Write> {
    uart: Arc<Mutex<Uart<W>>>,
    input: Option<AsyncInput>,
    drained: Arc<Notify>,
    pending: SerialInputState,
}

impl<W: Write> SerialDevice<W> {
    /// A UART that feeds `input`, if any, to the guest and writes guest output
    /// to `output`, which must not block. Registers `input` with the current
    /// thread's reactor.
    pub fn new(input: Option<OwnedFd>, output: W) -> Result<(Self, SerialPort<W>), SerialError> {
        Self::restore(&SerialSnapshot::default(), input, output)
    }

    /// Restore with newly supplied host endpoints. Validate before any live
    /// effects, preserve UART registers verbatim, and leave the IRQ fd empty.
    /// Spawn the returned task parked, then explicitly reraise pending IRQs
    /// after restoring the CPU/LAPIC/clock and before resuming it.
    pub fn restore(
        snapshot: &SerialSnapshot,
        input: Option<OwnedFd>,
        output: W,
    ) -> Result<(Self, SerialPort<W>), SerialError> {
        snapshot.validate()?;
        let irq = EventFd::new(EFD_NONBLOCK | EFD_CLOEXEC).map_err(SerialError::Irq)?;
        let irq_fd = irq.try_clone().map_err(SerialError::Irq)?;
        let drained = Arc::new(Notify::new());
        let events = FifoEvents {
            drained: Arc::clone(&drained),
        };
        let uart = Serial::from_state(
            &snapshot.uart,
            IrqLine {
                fd: irq,
                armed: Cell::new(false),
            },
            events,
            output,
        )
        .map_err(SerialError::Restore)?;
        uart.interrupt_evt().armed.set(true);
        let uart = Arc::new(Mutex::new(uart));
        let input = input
            .map(AsyncInput::new)
            .transpose()
            .map_err(SerialError::RegisterInput)?;
        let port = SerialPort {
            uart: Arc::clone(&uart),
            irq: irq_fd,
        };
        let device = Self {
            uart,
            input,
            drained,
            pending: snapshot.input.clone(),
        };
        Ok((device, port))
    }

    /// Feeds saved pending bytes first, even without a new host input, then
    /// reads host input until stopped or EOF. Quiesce reports precisely the
    /// bytes read but not enqueued and parks without touching the UART.
    pub async fn run(mut self, mut control: Control<SerialInputState>) -> Result<(), SerialError> {
        loop {
            select! {
                biased;
                command = control.recv() => {
                    if control.handle(command, || self.pending.clone()).await == Flow::Stop {
                        return Ok(());
                    }
                }
                more = self.feed() => {
                    if !more? {
                        info_unlimited!("serial host input closed");
                        return Ok(());
                    }
                }
            }
        }
    }

    /// One cancel-safe step. Reads commit their bytes synchronously, and FIFO
    /// enqueues commit before another await. Control is polled before even an
    /// immediately ready step, including restored input on a parked start.
    async fn feed(&mut self) -> Result<bool, SerialError> {
        if !self.pending.pending.is_empty() {
            let sent = {
                let mut uart = self.lock()?;
                if uart.fifo_capacity() == 0 {
                    None
                } else {
                    Some(
                        uart.enqueue_raw_bytes(&self.pending.pending)
                            .map_err(SerialError::Enqueue)?,
                    )
                }
            };
            if let Some(sent) = sent {
                self.pending.pending.drain(..sent);
                if sent == 0 {
                    // vm-superio accepts no host bytes in loopback mode. Keep
                    // them pending and let control run until the guest exits it.
                    tokio::task::yield_now().await;
                }
            } else {
                self.drained.notified().await;
            }
        } else if let Some(input) = &self.input {
            let mut buf = [0u8; RX_CHUNK];
            let filled = input.read(&mut buf).await.map_err(SerialError::Read)?;
            if filled == 0 {
                return Ok(false);
            }
            self.pending.pending.extend_from_slice(&buf[..filled]);
        } else {
            // Saved pending input has drained; only control can wake this task.
            return std::future::pending().await;
        }
        Ok(true)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Uart<W>>, SerialError> {
        self.uart.lock().map_err(|_| SerialError::Poisoned)
    }
}
