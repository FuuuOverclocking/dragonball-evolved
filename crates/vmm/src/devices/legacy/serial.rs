//! A 16550A UART: registers on the PIO bus, host input fed by its own task.
//!
//! Back-pressure is a position in [`SerialDevice::run`]: while the guest's
//! receive FIFO is full the task does not read host input, and the guest
//! draining the FIFO wakes it.

use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex, MutexGuard};

use logger::{info_unlimited, warn};
use tokio::select;
use tokio::sync::Notify;
use vm_superio::serial::{Error as UartError, SerialEvents};
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

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum SerialError {
    /// create the serial interrupt eventfd
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
struct IrqLine(EventFd);

impl Trigger for IrqLine {
    type E = io::Error;

    fn trigger(&self) -> io::Result<()> {
        self.0.write(1)
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
}

impl<W: Write> SerialDevice<W> {
    /// A UART that feeds `input`, if any, to the guest and writes guest output
    /// to `output`, which must not block. Registers `input` with the current
    /// thread's reactor.
    pub fn new(input: Option<OwnedFd>, output: W) -> Result<(Self, SerialPort<W>), SerialError> {
        let irq = EventFd::new(EFD_NONBLOCK | EFD_CLOEXEC).map_err(SerialError::Irq)?;
        let irq_fd = irq.try_clone().map_err(SerialError::Irq)?;
        let drained = Arc::new(Notify::new());
        let events = FifoEvents {
            drained: Arc::clone(&drained),
        };
        let uart = Arc::new(Mutex::new(Serial::with_events(
            IrqLine(irq),
            events,
            output,
        )));
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
        };
        Ok((device, port))
    }

    /// Feeds host input to the guest until stopped or the input closes.
    /// Quiesced, it neither reads input nor touches the FIFO.
    pub async fn run(self, mut control: Control<()>) -> Result<(), SerialError> {
        let Some(input) = &self.input else {
            loop {
                let command = control.recv().await;
                if control.handle(command, || ()).await == Flow::Stop {
                    return Ok(());
                }
            }
        };
        let mut buf = [0u8; RX_CHUNK];
        // Bytes read from the host, and how many of them the FIFO has taken.
        let mut filled = 0;
        let mut sent = 0;
        loop {
            if sent == filled {
                select! {
                    biased;
                    command = control.recv() => {
                        if control.handle(command, || ()).await == Flow::Stop {
                            return Ok(());
                        }
                        continue;
                    }
                    read = input.read(&mut buf) => {
                        filled = read.map_err(SerialError::Read)?;
                        sent = 0;
                    }
                }
                if filled == 0 {
                    info_unlimited!("serial host input closed");
                    return Ok(());
                }
            }
            // Not reading host input is the whole back-pressure mechanism.
            if self.lock()?.fifo_capacity() == 0 {
                select! {
                    biased;
                    command = control.recv() => {
                        if control.handle(command, || ()).await == Flow::Stop {
                            return Ok(());
                        }
                    }
                    () = self.drained.notified() => {}
                }
                continue;
            }
            sent += self
                .lock()?
                .enqueue_raw_bytes(&buf[sent..filled])
                .map_err(SerialError::Enqueue)?;
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, Uart<W>>, SerialError> {
        self.uart.lock().map_err(|_| SerialError::Poisoned)
    }
}
