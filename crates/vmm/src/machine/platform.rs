//! The legacy platform devices: the i8042, whose reset line is how the guest
//! reboots, and COM1, which carries the console when the plan has one.

use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::arch::x86_64::layout::{COM1_GSI, COM1_PORTS, I8042_PORTS};
use crate::bus::BusDevice;
use crate::devices::legacy::console::{self, ConsoleError, ConsoleInput, RawModeGuard};
use crate::devices::legacy::i8042::I8042Device;
use crate::devices::legacy::serial::{SerialDevice, SerialError};
use crate::devices::runtime::{DeviceHandle, DeviceRuntime, SpawnError, Start};
use crate::machine::MachineEvent;
use crate::space::{Pio, Range};
use crate::vm::KvmVm;

const CONSOLE_INPUT: &str = "console-input";
const CONSOLE_OUTPUT: &str = "console-output";

pub(super) type PioDevices = Vec<(Range<Pio>, Arc<dyn BusDevice<Pio>>)>;

/// Where COM1 meets the host.
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
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum PlatformError {
    /// open the console input
    ConsoleInput(#[source] ConsoleError),
    /// create COM1
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

/// The platform's device tasks, and the terminal mode they need.
#[derive(Debug)]
pub(super) struct Platform {
    devices: Vec<DeviceHandle<()>>,
    // Restores the terminal once the platform is gone.
    _raw_mode: Option<RawModeGuard>,
}

impl Platform {
    /// Puts the i8042, and COM1 if `com1`, on the PIO bus. COM1's tasks start
    /// parked.
    pub(super) fn attach(
        com1: bool,
        console: ConsoleBackend,
        vm: &KvmVm,
        events: &mpsc::UnboundedSender<MachineEvent>,
        runtime: &mut DeviceRuntime,
        pio: &mut PioDevices,
    ) -> Result<Self, PlatformError> {
        pio.push((I8042_PORTS, Arc::new(I8042Device::new(events.clone()))));
        let mut platform = Self {
            devices: Vec::new(),
            _raw_mode: None,
        };
        if !com1 {
            return Ok(platform);
        }
        match console {
            ConsoleBackend::Stdio => {
                let (input, raw_mode) =
                    match console::open_input().map_err(PlatformError::ConsoleInput)? {
                        Some(ConsoleInput { fd, raw_mode }) => (Some(fd), raw_mode),
                        None => (None, None),
                    };
                platform._raw_mode = raw_mode;
                let (output, drain) = console::output(console::DEFAULT_OUTPUT_CAPACITY);
                let serial = attach_serial(input, output, vm, runtime, pio)?;
                platform.devices.push(serial);
                let drain = runtime
                    .spawn(CONSOLE_OUTPUT, Start::Parked, move |control| {
                        drain.run_stdout(control)
                    })
                    .map_err(|source| PlatformError::Spawn {
                        name: CONSOLE_OUTPUT,
                        source,
                    })?;
                platform.devices.push(drain);
            }
            #[cfg(test)]
            ConsoleBackend::Captured(output) => {
                let serial = attach_serial(None, output, vm, runtime, pio)?;
                platform.devices.push(serial);
            }
        }
        Ok(platform)
    }

    /// The device tasks, in the order they were attached.
    pub(super) fn devices(&self) -> &[DeviceHandle<()>] {
        &self.devices
    }
}

/// Puts COM1 on the PIO bus and its interrupt on its GSI, and starts its
/// input task parked.
fn attach_serial<W: Write + Send + 'static>(
    input: Option<OwnedFd>,
    output: W,
    vm: &KvmVm,
    runtime: &mut DeviceRuntime,
    pio: &mut PioDevices,
) -> Result<DeviceHandle<()>, PlatformError> {
    let (serial, port) = SerialDevice::new(input, output).map_err(PlatformError::Serial)?;
    vm.fd()
        .register_irqfd(port.irq_fd(), COM1_GSI.0)
        .map_err(PlatformError::Irqfd)?;
    let handle = runtime
        .spawn(CONSOLE_INPUT, Start::Parked, move |control| {
            serial.run(control)
        })
        .map_err(|source| PlatformError::Spawn {
            name: CONSOLE_INPUT,
            source,
        })?;
    pio.push((COM1_PORTS, Arc::new(port)));
    Ok(handle)
}
