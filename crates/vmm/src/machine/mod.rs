//! A machine: planned from its spec ([`plan`]), built paused ([`build`]),
//! then moved between [`Machine`] and [`PausedMachine`] until torn down.

mod block;
mod boot;
mod build;
mod cleanup;
mod kvm_state;
mod mem;
pub mod plan;
mod platform;
mod snapshot;
mod state;
#[cfg(test)]
mod tests;
mod vcpu;

use tokio::sync::mpsc;

pub use self::block::{AttachBlockError, BlockSnapshot, CaptureBlockError, RestoreBlockQueueError};
pub use self::boot::BootDataError;
pub use self::build::{BuildCleanup, BuildError, BuildFailure, build};
pub use self::cleanup::PendingCleanup;
pub use self::kvm_state::{CaptureVmError, RestoreVmError, VcpuSnapshot, VmKvmState};
pub use self::mem::{AttachMemError, MachineResizeError, MemSnapshot};
pub use self::plan::{MachinePlan, PlanError};
pub use self::platform::{
    PlatformCaptureError, PlatformError, PlatformQuiesced, PlatformRestoreError, PlatformSnapshot,
};
pub use self::snapshot::{
    CaptureError, DeviceSnapshot, MachineSnapshot, RebuildError, RebuildFailure, RebuildMemory,
    rebuild,
};
pub use self::state::{
    FailedMachine, Machine, PauseError, PausedMachine, QuiescedBlock, QuiescedDevice, QuiescedMem,
    ResumeError, TeardownError,
};
pub use self::vcpu::{
    CaptureStep, CaptureVcpuError, ConfigureVcpuError, RestoreStep, RestoreVcpuError, SetupStep,
    SpawnVcpuError, VcpuError,
};
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
