//! A machine: planned from its spec ([`plan`]), built paused ([`build`]),
//! then moved between [`Machine`] and [`PausedMachine`] until torn down.

mod block;
mod boot;
mod build;
pub mod plan;
mod platform;
mod state;
#[cfg(test)]
mod tests;
mod vcpu;

use tokio::sync::mpsc;

pub use self::block::AttachBlockError;
pub use self::boot::BootDataError;
pub use self::build::{BuildError, build};
pub use self::plan::{MachinePlan, PlanError};
pub use self::platform::PlatformError;
pub use self::state::{
    Machine, PauseError, PausedMachine, QuiescedBlock, QuiescedDevice, ResumeError, TeardownError,
};
pub use self::vcpu::{ConfigureVcpuError, SetupStep, SpawnVcpuError, VcpuError};
use crate::devices::runtime::DeviceExits;

/// An event from a vCPU thread or a platform device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineEvent {
    /// The guest asked for a reset, which ends the machine.
    GuestReset,
    /// A vCPU thread failed; `error` is its rendered source chain.
    VcpuFailed { index: u8, error: String },
}

/// What a machine reports while it lives. It is kept apart from the machine,
/// so whoever drives the machine can listen while a transition owns it.
#[derive(Debug)]
pub struct MachineEvents {
    /// Guest resets and vCPU failures. Closes once the machine is gone and
    /// every vCPU thread has ended.
    pub machine: mpsc::UnboundedReceiver<MachineEvent>,
    /// Device tasks that ended without being asked to.
    pub devices: DeviceExits,
}
