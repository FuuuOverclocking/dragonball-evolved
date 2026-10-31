//! The legacy platform devices: the i8042, whose reset line is how the guest
//! reboots, and COM1, which carries the console when the plan has one.

use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::arch::x86_64::layout::{COM1_GSI, COM1_PORTS, I8042_PORTS};
use crate::bus::BusDevice;
use crate::devices::legacy::console::{self, ConsoleError, ConsoleInput, ConsoleOut, RawModeGuard};
use crate::devices::legacy::i8042::{I8042Device, I8042State};
use crate::devices::legacy::serial::{
    SerialDevice, SerialError, SerialInputState, SerialPort, SerialSnapshot, SerialSnapshotError,
};
use crate::devices::runtime::{DeviceHandle, DeviceRuntime, Response, SpawnError, Start};
use crate::machine::MachineEvent;
use crate::space::{Pio, Range};
use crate::vm::KvmVm;

// Task names for diagnostics only, never for classifying snapshot participants.
const CONSOLE_INPUT: &str = "console-input";
const CONSOLE_OUTPUT: &str = "console-output";

pub(super) type PioDevices = Vec<(Range<Pio>, Arc<dyn BusDevice<Pio>>)>;

/// Where COM1 meets the host. Host endpoints are supplied anew on restore.
#[derive(Debug)]
pub(super) enum ConsoleBackend {
    /// The process's stdin and stdout.
    Stdio,
    /// Guest output into a buffer, and no input.
    #[cfg(test)]
    Captured(Captured),
}

/// Guest console output, as the host would see it.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub(super) struct Captured(pub(super) Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(test)]
impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Only the UART's output varies; the port/capture capability stays concrete.
#[derive(Debug)]
enum ConsoleOutput {
    Buffered(ConsoleOut),
    #[cfg(test)]
    Captured(Captured),
}

impl Write for ConsoleOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Buffered(output) => output.write(buf),
            #[cfg(test)]
            Self::Captured(output) => output.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Buffered(output) => output.flush(),
            #[cfg(test)]
            Self::Captured(output) => output.flush(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformSnapshot {
    pub i8042: I8042State,
    pub serial: Option<SerialSnapshot>,
}

impl PlatformSnapshot {
    /// Validate before rebinding host endpoints or mutating the bus.
    pub fn validate(&self, com1: bool) -> Result<(), PlatformRestoreError> {
        if com1 != self.serial.is_some() {
            return Err(PlatformRestoreError::Com1);
        }
        if let Some(serial) = &self.serial {
            serial.validate().map_err(PlatformRestoreError::Serial)?;
        }
        Ok(())
    }
}

/// Keep input's pending-byte reply separate from the host-only drain outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformQuiesced {
    pub serial: Option<Response<SerialInputState>>,
    pub output: Option<Response<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformResumed {
    pub serial: Option<Response<()>>,
    pub output: Option<Response<()>>,
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum PlatformRestoreError {
    /// saved COM1 presence does not match the machine plan
    Com1,
    /// invalid saved COM1 state
    Serial(#[source] SerialSnapshotError),
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum PlatformCaptureError {
    /// COM1 input has not replied to quiesce
    SerialNotQuiesced,
    /// COM1 input has exited
    SerialExited,
    /// COM1 input did not quiesce in time
    SerialTimedOut,
    /// quiesce outcome does not match this platform's COM1 presence
    Com1,
    /// capture COM1 UART state
    Serial(#[source] SerialError),
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum PlatformError {
    /// invalid saved platform state
    Restore(#[from] PlatformRestoreError),
    /// open the console input
    ConsoleInput(#[source] ConsoleError),
    /// create or signal COM1
    Serial(#[source] SerialError),
    /// connect COM1 to its interrupt line
    Irqfd(#[source] kvm_ioctls::Error),
    /// start device {name}
    Spawn {
        name: &'static str,
        #[source]
        source: SpawnError,
    },
}

#[derive(Debug)]
struct AttachedSerial {
    input: DeviceHandle<SerialInputState>,
    port: Arc<SerialPort<ConsoleOutput>>,
}

#[derive(Debug)]
pub(super) struct Platform {
    i8042: Arc<I8042Device>,
    serial: Option<AttachedSerial>,
    output: Option<DeviceHandle<()>>,
    // Restores the terminal once the platform is gone.
    _raw_mode: Option<RawModeGuard>,
}

impl Platform {
    /// Keep input/IRQs parked until CPU/LAPIC/clock restore; resume the platform before vCPUs.
    pub(super) fn restore(
        com1: bool,
        snapshot: &PlatformSnapshot,
        console: ConsoleBackend,
        vm: &KvmVm,
        events: &mpsc::UnboundedSender<MachineEvent>,
        runtime: &mut DeviceRuntime,
        pio: &mut PioDevices,
    ) -> Result<Self, PlatformError> {
        snapshot.validate(com1)?;
        let mut platform = Self {
            i8042: Arc::new(I8042Device::from_state(snapshot.i8042, events.clone())),
            serial: None,
            output: None,
            _raw_mode: None,
        };
        if let Some(serial) = &snapshot.serial {
            match console {
                ConsoleBackend::Stdio => {
                    let (input, raw_mode) =
                        match console::open_input().map_err(PlatformError::ConsoleInput)? {
                            Some(ConsoleInput { fd, raw_mode }) => (Some(fd), raw_mode),
                            None => (None, None),
                        };
                    platform._raw_mode = raw_mode;
                    let (output, drain) = console::output(console::DEFAULT_OUTPUT_CAPACITY);
                    platform.serial = Some(attach_serial(
                        serial,
                        input,
                        ConsoleOutput::Buffered(output),
                        vm,
                        runtime,
                    )?);
                    platform.output = Some(
                        runtime
                            .spawn(CONSOLE_OUTPUT, Start::Parked, move |control| {
                                drain.run_stdout(control)
                            })
                            .map_err(|source| PlatformError::Spawn {
                                name: CONSOLE_OUTPUT,
                                source,
                            })?,
                    );
                }
                #[cfg(test)]
                ConsoleBackend::Captured(output) => {
                    platform.serial = Some(attach_serial(
                        serial,
                        None,
                        ConsoleOutput::Captured(output),
                        vm,
                        runtime,
                    )?);
                }
            }
        }
        pio.push((I8042_PORTS, platform.i8042.clone()));
        if let Some(serial) = &platform.serial {
            pio.push((COM1_PORTS, serial.port.clone()));
        }
        Ok(platform)
    }

    /// Host-only output failure must not invalidate the serial-input reply.
    pub(super) async fn quiesce(&self, within: Duration) -> PlatformQuiesced {
        let deadline = Instant::now() + within;
        let serial = match &self.serial {
            Some(serial) => Some(serial.input.quiesce(remaining(deadline)).await),
            None => None,
        };
        let output = match &self.output {
            Some(output) => Some(output.quiesce(remaining(deadline)).await),
            None => None,
        };
        PlatformQuiesced { serial, output }
    }

    pub(super) async fn resume(&self, within: Duration) -> PlatformResumed {
        let deadline = Instant::now() + within;
        let serial = match &self.serial {
            Some(serial) => Some(serial.input.resume(remaining(deadline)).await),
            None => None,
        };
        let output = match &self.output {
            Some(output) => Some(output.resume(remaining(deadline)).await),
            None => None,
        };
        PlatformResumed { serial, output }
    }

    /// Keep CPUs parked and input quiesced through capture; host-only output does not gate it.
    pub(super) fn capture(
        &self,
        quiesced: &PlatformQuiesced,
    ) -> Result<PlatformSnapshot, PlatformCaptureError> {
        let serial = match (&self.serial, &quiesced.serial) {
            (None, None) => None,
            (Some(_), None) => return Err(PlatformCaptureError::SerialNotQuiesced),
            (None, Some(_)) => return Err(PlatformCaptureError::Com1),
            (Some(_), Some(Response::Exited)) => return Err(PlatformCaptureError::SerialExited),
            (Some(_), Some(Response::TimedOut)) => {
                return Err(PlatformCaptureError::SerialTimedOut);
            }
            (Some(serial), Some(Response::Replied(input))) => {
                if serial.input.has_exited() {
                    return Err(PlatformCaptureError::SerialExited);
                }
                let uart = serial
                    .port
                    .capture_uart()
                    .map_err(PlatformCaptureError::Serial)?;
                if serial.input.has_exited() {
                    return Err(PlatformCaptureError::SerialExited);
                }
                Some(SerialSnapshot {
                    uart,
                    input: input.clone(),
                })
            }
        };
        Ok(PlatformSnapshot {
            i8042: self.i8042.snapshot(),
            serial,
        })
    }

    /// Reassert COM1's saved pending IRQ only at the restore ordering boundary.
    pub(super) fn reraise_pending(&self) -> Result<(), PlatformError> {
        if let Some(serial) = &self.serial {
            serial
                .port
                .reraise_pending()
                .map_err(PlatformError::Serial)?;
        }
        Ok(())
    }
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// Saved pending bytes must wait for the first input resume.
fn attach_serial(
    snapshot: &SerialSnapshot,
    input: Option<OwnedFd>,
    output: ConsoleOutput,
    vm: &KvmVm,
    runtime: &mut DeviceRuntime,
) -> Result<AttachedSerial, PlatformError> {
    let (serial, port) =
        SerialDevice::restore(snapshot, input, output).map_err(PlatformError::Serial)?;
    vm.fd()
        .register_irqfd(port.irq_fd(), COM1_GSI.0)
        .map_err(PlatformError::Irqfd)?;
    let input = runtime
        .spawn(CONSOLE_INPUT, Start::Parked, move |control| {
            serial.run(control)
        })
        .map_err(|source| PlatformError::Spawn {
            name: CONSOLE_INPUT,
            source,
        })?;
    Ok(AttachedSerial {
        input,
        port: Arc::new(port),
    })
}

#[cfg(test)]
mod tests {
    use tokio::time;

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn platform(runtime: &mut DeviceRuntime) -> Platform {
        let (events, _receiver) = mpsc::unbounded_channel();
        let (output, _drain) = console::output(64);
        let (device, port) = SerialDevice::new(None, ConsoleOutput::Buffered(output)).unwrap();
        let input = runtime
            .spawn(CONSOLE_INPUT, Start::Parked, move |control| {
                device.run(control)
            })
            .unwrap();
        Platform {
            i8042: Arc::new(I8042Device::new(events)),
            serial: Some(AttachedSerial {
                input,
                port: Arc::new(port),
            }),
            output: None,
            _raw_mode: None,
        }
    }

    #[test]
    fn validation_checks_com1_and_both_byte_bounds() {
        let mut saved = PlatformSnapshot {
            i8042: I8042State::default(),
            serial: None,
        };
        assert!(saved.validate(false).is_ok());
        assert!(matches!(
            saved.validate(true),
            Err(PlatformRestoreError::Com1)
        ));
        saved.serial = Some(SerialSnapshot::default());
        assert!(matches!(
            saved.validate(false),
            Err(PlatformRestoreError::Com1)
        ));
        saved.serial.as_mut().unwrap().uart.in_buffer = vec![0; 65];
        assert!(matches!(
            saved.validate(true),
            Err(PlatformRestoreError::Serial(
                SerialSnapshotError::FifoLength(65)
            ))
        ));
        saved.serial.as_mut().unwrap().uart.in_buffer.clear();
        saved.serial.as_mut().unwrap().input.pending = vec![0; 65];
        assert!(matches!(
            saved.validate(true),
            Err(PlatformRestoreError::Serial(
                SerialSnapshotError::PendingLength(65)
            ))
        ));
    }

    #[tokio::test(flavor = "local")]
    async fn output_failure_or_timeout_does_not_prohibit_capture() {
        let (mut runtime, mut exits) = DeviceRuntime::new();
        let mut platform = platform(&mut runtime);
        platform.output = Some(
            runtime
                .spawn(CONSOLE_OUTPUT, Start::Running, |control| async move {
                    drop(control);
                    Err::<(), _>(io::Error::other("host output failed"))
                })
                .unwrap(),
        );
        // The supervisor's exit is the handshake, not a scheduling delay.
        let exit = time::timeout(TIMEOUT, exits.next()).await.unwrap().unwrap();
        assert_eq!(&*exit.name, CONSOLE_OUTPUT);
        assert!(exit.result.is_err());
        let mut quiesced = platform.quiesce(TIMEOUT).await;
        assert_eq!(
            quiesced.serial,
            Some(Response::Replied(SerialInputState::default()))
        );
        assert_eq!(quiesced.output, Some(Response::Exited));
        let saved = platform.capture(&quiesced).unwrap();
        quiesced.output = Some(Response::TimedOut);
        assert_eq!(platform.capture(&quiesced).unwrap(), saved);
        let resumed = platform.resume(TIMEOUT).await;
        assert_eq!(resumed.serial, Some(Response::Replied(())));
        assert_eq!(resumed.output, Some(Response::Exited));
        runtime.shutdown(TIMEOUT).await.unwrap();
    }

    #[tokio::test(flavor = "local")]
    async fn capture_requires_a_reply_and_a_still_alive_input_task() {
        let (mut runtime, _exits) = DeviceRuntime::new();
        let platform = platform(&mut runtime);
        let missing = PlatformQuiesced {
            serial: None,
            output: None,
        };
        assert!(matches!(
            platform.capture(&missing),
            Err(PlatformCaptureError::SerialNotQuiesced)
        ));
        let timed_out = PlatformQuiesced {
            serial: Some(Response::TimedOut),
            output: None,
        };
        assert!(matches!(
            platform.capture(&timed_out),
            Err(PlatformCaptureError::SerialTimedOut)
        ));
        let quiesced = platform.quiesce(TIMEOUT).await;
        assert!(platform.capture(&quiesced).is_ok());
        let serial = platform.serial.as_ref().unwrap();
        serial.input.stop();
        time::timeout(TIMEOUT, serial.input.finished())
            .await
            .unwrap();
        assert!(matches!(
            platform.capture(&quiesced),
            Err(PlatformCaptureError::SerialExited)
        ));
        runtime.shutdown(TIMEOUT).await.unwrap();
    }
}
