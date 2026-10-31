//! A built machine: running ([`Machine`]), paused ([`PausedMachine`]), or
//! teardown-only after a CPU transition failure ([`FailedMachine`]).
//!
//! Transitions consume their source state. Only acknowledged CPU transitions
//! produce a usable destination state. They are not meant to be cancelled:
//! every wait inside them is bounded by a timeout, so they always finish.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use logger::warn_unlimited;
use tokio::time::Instant;

use super::block::AttachedBlock;
use super::cleanup::PendingCleanup;
use super::plan::MachinePlan;
use super::platform::{Platform, PlatformQuiesced};
use super::vcpu::{VcpuError, Vcpus};
use crate::devices::runtime::{DeviceRuntime, Response, ShutdownError};
use crate::devices::virtio::block::BlockQuiesced;
use crate::memory::store::{MemoryStore, MemoryView};
use crate::vm::{Binding, KvmVm, MemSlots};

/// How long the vCPUs have to answer a pause, a resume or a stop.
const VCPU_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the devices together have to answer a quiesce or a resume.
const DEVICE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the devices have to stop at teardown.
const DEVICE_GRACE: Duration = Duration::from_secs(2);

/// pause the vCPUs
#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub struct PauseError(#[source] pub VcpuError);

/// resume the vCPUs
#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub struct ResumeError(#[source] pub VcpuError);

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum TeardownError {
    /// stop the vCPUs
    Vcpus(#[source] VcpuError),
    /// stop the devices
    Devices(#[source] ShutdownError),
    /// the VM is still held after teardown
    VmHeld,
}

/// What one device answered when its machine paused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuiescedDevice {
    pub name: Arc<str>,
    /// `Exited` and `TimedOut` mark a device that may not be quiescent.
    pub response: Response<()>,
}

/// The two tasks of one block device, recorded under its plan ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuiescedBlock {
    pub worker: Response<BlockQuiesced>,
    pub resample: Response<()>,
}

/// Everything a built machine owns, whatever its state. Fields drop in order,
/// so the vCPUs are told to stop before anything they reach goes away.
#[derive(Debug)]
pub(super) struct Parts {
    pub(super) vcpus: Vcpus,
    pub(super) runtime: DeviceRuntime,
    pub(super) platform: Platform,
    pub(super) blocks: BTreeMap<String, AttachedBlock>,
    pub(super) bindings: Vec<Binding>,
    pub(super) memslots: MemSlots,
    pub(super) vm: Arc<KvmVm>,
    pub(super) plan: MachinePlan,
}

impl Parts {
    async fn teardown(self: Box<Self>) -> Result<MemoryStore, (PendingCleanup, TeardownError)> {
        PendingCleanup::from_parts(self)
            .wait(VCPU_TIMEOUT + DEVICE_GRACE)
            .await
    }

    /// Asks every device to quiesce, within one shared deadline. A device
    /// that has exited or does not answer is recorded, and the rest go on.
    pub(super) async fn quiesce_devices(
        &self,
    ) -> (PlatformQuiesced, BTreeMap<String, QuiescedBlock>) {
        let deadline = Instant::now() + DEVICE_TIMEOUT;
        let mut workers = BTreeMap::new();
        for (id, block) in &self.blocks {
            let response = block
                .worker
                .quiesce(deadline.saturating_duration_since(Instant::now()))
                .await;
            warn_response(block.worker.name(), &response, "quiesce");
            workers.insert(id.clone(), response);
        }
        let mut blocks = BTreeMap::new();
        for (id, block) in &self.blocks {
            let resample = block
                .resample
                .quiesce(deadline.saturating_duration_since(Instant::now()))
                .await;
            warn_response(block.resample.name(), &resample, "quiesce");
            blocks.insert(
                id.clone(),
                QuiescedBlock {
                    worker: workers[id],
                    resample,
                },
            );
        }
        let platform = self
            .platform
            .quiesce(deadline.saturating_duration_since(Instant::now()))
            .await;
        if let Some(response) = &platform.serial {
            warn_response("console-input", response, "quiesce");
        }
        if let Some(response) = &platform.output {
            warn_response("console-output", response, "quiesce");
        }
        (platform, blocks)
    }

    /// Asks every device to resume, within one shared deadline. A device that
    /// has exited or does not answer is left as it is.
    async fn resume_devices(&self) {
        let deadline = Instant::now() + DEVICE_TIMEOUT;
        for block in self.blocks.values() {
            let response = block
                .resample
                .resume(deadline.saturating_duration_since(Instant::now()))
                .await;
            warn_response(block.resample.name(), &response, "resume");
        }
        for block in self.blocks.values() {
            let response = block
                .worker
                .resume(deadline.saturating_duration_since(Instant::now()))
                .await;
            warn_response(block.worker.name(), &response, "resume");
        }
        let platform = self
            .platform
            .resume(deadline.saturating_duration_since(Instant::now()))
            .await;
        if let Some(response) = &platform.serial {
            warn_response("console-input", response, "resume");
        }
        if let Some(response) = &platform.output {
            warn_response("console-output", response, "resume");
        }
    }
}

fn platform_diagnostics(platform: &PlatformQuiesced) -> Vec<QuiescedDevice> {
    fn erased<Q>(response: &Response<Q>) -> Response<()> {
        match response {
            Response::Replied(_) => Response::Replied(()),
            Response::Exited => Response::Exited,
            Response::TimedOut => Response::TimedOut,
        }
    }
    let mut devices = Vec::new();
    if let Some(response) = &platform.serial {
        devices.push(QuiescedDevice {
            name: "console-input".into(),
            response: erased(response),
        });
    }
    if let Some(response) = &platform.output {
        devices.push(QuiescedDevice {
            name: "console-output".into(),
            response: *response,
        });
    }
    devices
}

fn warn_response<Q>(name: &str, response: &Response<Q>, operation: &str) {
    match response {
        Response::Replied(_) => {}
        Response::Exited => warn_unlimited!("device {name} has exited; continuing without it"),
        Response::TimedOut => {
            warn_unlimited!("device {name} did not {operation} in time; continuing without it")
        }
    }
}

/// A machine whose vCPUs run.
///
/// Only a paused machine records what its devices answered when quiesced, and
/// only a paused machine resumes:
///
/// ```compile_fail,E0599
/// fn quiesced(machine: &vmm::machine::Machine) {
///     let _ = machine.quiesced();
/// }
/// ```
///
/// ```compile_fail,E0599
/// async fn resume(machine: vmm::machine::Machine) {
///     let _ = machine.resume().await;
/// }
/// ```
///
/// ```compile_fail,E0599
/// async fn capture(machine: &vmm::machine::Machine) {
///     let _ = machine.capture().await;
/// }
/// ```
///
/// Dropping a machine instead of tearing it down stops it without waiting:
/// vCPU threads are asked to stop and keep the memory mapped until they have.
#[derive(Debug)]
pub struct Machine {
    // Boxed, so a transition moves a pointer, and hands one back beside its error.
    parts: Box<Parts>,
}

impl Machine {
    /// Pauses the vCPUs, then quiesces the devices.
    ///
    /// A CPU failure leaves its state unknown and returns teardown-only ownership.
    /// No compensating command can prove a state without an acknowledgement.
    ///
    /// A device that has exited or does not answer does not fail the pause:
    /// it is recorded in [`PausedMachine::quiesced`] and the pause completes.
    pub async fn pause(mut self) -> Result<PausedMachine, (FailedMachine, PauseError)> {
        if let Err(error) = self.parts.vcpus.pause(VCPU_TIMEOUT).await {
            return Err((FailedMachine { parts: self.parts }, PauseError(error)));
        }
        let (platform, blocks) = self.parts.quiesce_devices().await;
        Ok(PausedMachine {
            parts: self.parts,
            quiesced: platform_diagnostics(&platform),
            platform,
            blocks,
        })
    }

    /// Stops the vCPUs and the devices, closes the VM and hands its memory
    /// back; see [`PausedMachine::teardown`].
    pub async fn teardown(self) -> Result<MemoryStore, (PendingCleanup, TeardownError)> {
        self.parts.teardown().await
    }

    pub fn plan(&self) -> &MachinePlan {
        &self.parts.plan
    }

    pub fn memory(&self) -> &MemoryView {
        self.parts.vm.memory().view()
    }

    pub fn vcpu_count(&self) -> usize {
        self.parts.vcpus.len()
    }
}

/// A machine with unproven CPU state after a failed transition.
///
/// It privately owns all resources and can only be torn down:
///
/// ```compile_fail,E0599
/// async fn resume(machine: vmm::machine::FailedMachine) {
///     let _ = machine.resume().await;
/// }
/// ```
///
/// ```compile_fail,E0599
/// async fn pause(machine: vmm::machine::FailedMachine) {
///     let _ = machine.pause().await;
/// }
/// ```
///
/// ```compile_fail,E0599
/// async fn capture(machine: &vmm::machine::FailedMachine) {
///     let _ = machine.capture().await;
/// }
/// ```
///
/// ```compile_fail,E0308
/// fn needs_paused(_: vmm::machine::PausedMachine) {}
/// fn failed(machine: vmm::machine::FailedMachine) {
///     needs_paused(machine);
/// }
/// ```
#[derive(Debug)]
pub struct FailedMachine {
    parts: Box<Parts>,
}

impl FailedMachine {
    /// Stops the CPUs and devices using the same teardown as a live machine.
    /// Memory is returned only after successful cleanup; see [`PausedMachine::teardown`].
    pub async fn teardown(self) -> Result<MemoryStore, (PendingCleanup, TeardownError)> {
        self.parts.teardown().await
    }
}

/// A machine whose vCPUs are parked and whose devices are quiesced, as far as
/// they answered.
#[derive(Debug)]
pub struct PausedMachine {
    pub(super) parts: Box<Parts>,
    quiesced: Vec<QuiescedDevice>,
    platform: PlatformQuiesced,
    blocks: BTreeMap<String, QuiescedBlock>,
}

impl PausedMachine {
    /// A machine just built: vCPU threads and device tasks all started parked.
    pub(super) fn built(parts: Box<Parts>, platform: PlatformQuiesced) -> Self {
        let quiesced = platform_diagnostics(&platform);
        let blocks = parts
            .blocks
            .keys()
            .map(|id| {
                (
                    id.clone(),
                    QuiescedBlock {
                        worker: Response::Replied(BlockQuiesced::Inactive),
                        resample: Response::Replied(()),
                    },
                )
            })
            .collect();
        Self {
            parts,
            quiesced,
            platform,
            blocks,
        }
    }

    pub(super) fn record_quiesced(
        &mut self,
        platform: PlatformQuiesced,
        blocks: BTreeMap<String, QuiescedBlock>,
    ) {
        self.quiesced = platform_diagnostics(&platform);
        self.platform = platform;
        self.blocks = blocks;
    }

    pub(super) fn into_cleanup(self) -> PendingCleanup {
        PendingCleanup::from_parts(self.parts)
    }

    /// Resumes the devices, then the vCPUs.
    ///
    /// A device that has exited or does not answer is left as it is. A CPU
    /// failure returns teardown-only ownership, never a restored paused state.
    pub async fn resume(self) -> Result<Machine, (FailedMachine, ResumeError)> {
        // Consume the paused proof before any resource is resumed.
        let Self { mut parts, .. } = self;
        parts.resume_devices().await;
        if let Err(error) = parts.vcpus.resume(VCPU_TIMEOUT).await {
            return Err((FailedMachine { parts }, ResumeError(error)));
        }
        Ok(Machine { parts })
    }

    /// What each device answered when the machine paused, in attach order.
    pub fn quiesced(&self) -> &[QuiescedDevice] {
        &self.quiesced
    }

    pub fn quiesced_platform(&self) -> &PlatformQuiesced {
        &self.platform
    }

    /// Typed block outcomes, keyed by the normalized configuration ID.
    pub fn quiesced_blocks(&self) -> &BTreeMap<String, QuiescedBlock> {
        &self.blocks
    }

    /// Stops the vCPUs and then the devices, closes the VM and hands its
    /// memory back.
    ///
    /// Cleanup failure retains ownership for a bounded [`PendingCleanup::wait`] retry.
    pub async fn teardown(self) -> Result<MemoryStore, (PendingCleanup, TeardownError)> {
        self.parts.teardown().await
    }

    pub fn plan(&self) -> &MachinePlan {
        &self.parts.plan
    }

    pub fn memory(&self) -> &MemoryView {
        self.parts.vm.memory().view()
    }

    pub fn vcpu_count(&self) -> usize {
        self.parts.vcpus.len()
    }
}
