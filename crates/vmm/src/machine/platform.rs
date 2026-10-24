//! The legacy platform devices: the i8042 controller, whose reset line is how
//! the guest reboots, and COM1 when the console is on stdio.

use std::range::Range;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use api::machine::ConsoleConfig;
use kvm_ioctls::VmFd;
use tokio::sync::mpsc;
use vm_superio::SerialState;

use super::MachineEvent;
use super::bus::BusDevice;
use crate::arch::x86_64::layout;
use crate::devices::legacy::console::{self, ConsoleOut, RawModeGuard};
use crate::devices::legacy::i8042::{I8042Device, I8042State};
use crate::devices::legacy::serial::{SerialDevice, SerialHandle};
use crate::devices::{DeviceManager, DeviceRole};

/// The platform devices' captured state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformSnapshot {
    pub i8042: I8042State,
    /// Present exactly when the console is on COM1.
    pub serial: Option<SerialState>,
}

pub(super) struct Platform {
    i8042: Arc<I8042Device>,
    serial: Option<Arc<SerialHandle<ConsoleOut>>>,
    /// Restores the terminal when the platform is dropped.
    _raw_mode: Option<RawModeGuard>,
}

impl Platform {
    /// Puts the i8042, and COM1 for a stdio console, on the PIO bus. Restoring
    /// brings both back as captured and requires the capture to match the
    /// console configuration.
    pub(super) fn attach(
        console: ConsoleConfig,
        vm: &VmFd,
        events: &mpsc::UnboundedSender<MachineEvent>,
        tasks: &mut DeviceManager,
        pio_bus: &mut Vec<(Range<u64>, Arc<dyn BusDevice>)>,
        restore: Option<PlatformSnapshot>,
    ) -> Result<Self> {
        let i8042 = Arc::new(match &restore {
            Some(captured) => I8042Device::from_state(captured.i8042, events.clone()),
            None => I8042Device::new(events.clone()),
        });
        pio_bus.push((
            layout::I8042_PORTS,
            Arc::clone(&i8042) as Arc<dyn BusDevice>,
        ));

        // `None` when building; when restoring, whatever COM1 state was captured.
        let captured_serial = restore.map(|captured| captured.serial);
        let (serial, raw_mode) = match (console, captured_serial) {
            (ConsoleConfig::Stdio, None) => attach_serial(None, vm, tasks, pio_bus)?,
            (ConsoleConfig::Stdio, Some(Some(state))) => {
                attach_serial(Some(&state), vm, tasks, pio_bus)?
            }
            (ConsoleConfig::Stdio, Some(None)) => {
                bail!("the snapshot has no COM1 state for the stdio console")
            }
            (ConsoleConfig::None, Some(Some(_))) => {
                bail!("the snapshot has COM1 state but the machine has no console")
            }
            (ConsoleConfig::None, _) => (None, None),
        };
        Ok(Self {
            i8042,
            serial,
            _raw_mode: raw_mode,
        })
    }

    pub(super) fn capture(&self) -> Result<PlatformSnapshot> {
        let serial = match &self.serial {
            Some(handle) => Some(
                handle
                    .uart
                    .lock()
                    .map_err(|_| anyhow!("serial uart mutex was poisoned"))?
                    .state(),
            ),
            None => None,
        };
        Ok(PlatformSnapshot {
            i8042: self.i8042.snapshot_state(),
            serial,
        })
    }
}

type SerialAttachment = (Option<Arc<SerialHandle<ConsoleOut>>>, Option<RawModeGuard>);

fn attach_serial(
    restore: Option<&SerialState>,
    vm: &VmFd,
    tasks: &mut DeviceManager,
    pio_bus: &mut Vec<(Range<u64>, Arc<dyn BusDevice>)>,
) -> Result<SerialAttachment> {
    let (input, raw_mode) = match console::open_input()? {
        Some(input) => (Some(input.fd), input.raw_mode),
        None => (None, None),
    };
    let (output, drain) = console::output(console::DEFAULT_OUTPUT_CAPACITY);
    let (serial, handle) = match restore {
        Some(state) => SerialDevice::from_state(state, input, output)?,
        None => SerialDevice::with_input(input, output)?,
    };
    let handle = Arc::new(handle);
    vm.register_irqfd(&handle.irq_fd, layout::COM1_GSI.0)?;
    pio_bus.push((
        layout::COM1_PORTS,
        Arc::clone(&handle) as Arc<dyn BusDevice>,
    ));
    let stop = tasks.stop_signal();
    tasks.spawn(
        "console-input",
        DeviceRole::GuestAffecting,
        serial.run(stop),
    );
    let stop = tasks.stop_signal();
    tasks.spawn(
        "console-output",
        DeviceRole::HostOnly,
        drain.run_stdout(stop),
    );
    Ok((Some(handle), raw_mode))
}
