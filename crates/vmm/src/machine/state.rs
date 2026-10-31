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
use super::plan::MachinePlan;
use super::platform::Platform;
use super::vcpu::{VcpuError, Vcpus};
use crate::devices::runtime::{DeviceRuntime, Response, ShutdownError};
use crate::devices::virtio::block::BlockQuiesced;
use crate::error::render_chain;
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
    /// Stops the vCPUs, then the devices; only once both have stopped is the
    /// memory handed back. A vCPU thread that does not stop keeps the VM, and
    /// so the memory, mapped until it ends; a device that does not stop may
    /// still reach the memory, which is therefore not handed back either.
    async fn teardown(self) -> Result<MemoryStore, TeardownError> {
        let Self {
            vcpus,
            runtime,
            platform,
            blocks,
            bindings,
            memslots,
            vm,
            plan: _,
        } = self;
        let vcpus = vcpus.stop(VCPU_TIMEOUT).await;
        let devices = runtime.shutdown(DEVICE_GRACE).await;
        // Restores the terminal, now that nothing serves the console.
        drop(platform);
        for block in blocks.into_values() {
            let AttachedBlock {
                worker,
                resample,
                transport,
            } = block;
            drop((transport, worker, resample));
        }
        drop(bindings);
        drop(memslots);
        match (vcpus, devices) {
            (Err(vcpus), devices) => {
                if let Err(devices) = devices {
                    warn_unlimited!("{}", render_chain(&devices));
                }
                Err(TeardownError::Vcpus(vcpus))
            }
            (Ok(()), Err(devices)) => Err(TeardownError::Devices(devices)),
            (Ok(()), Ok(())) => KvmVm::into_memory(vm).map_err(|_| TeardownError::VmHeld),
        }
    }

    /// Asks every device to quiesce, within one shared deadline. A device
    /// that has exited or does not answer is recorded, and the rest go on.
    async fn quiesce_devices(&self) -> (Vec<QuiescedDevice>, BTreeMap<String, QuiescedBlock>) {
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
        let mut quiesced = Vec::new();
        for device in self.platform.devices() {
            let response = device
                .quiesce(deadline.saturating_duration_since(Instant::now()))
                .await;
            let name = device.name();
            match response {
                Response::Replied(()) => {}
                Response::Exited => {
                    warn_unlimited!("device {name} has exited; pausing without it");
                }
                Response::TimedOut => {
                    warn_unlimited!("device {name} did not quiesce in time; pausing without it");
                }
            }
            quiesced.push(QuiescedDevice {
                name: name.into(),
                response,
            });
        }
        (quiesced, blocks)
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
        for device in self.platform.devices() {
            let name = device.name();
            match device
                .resume(deadline.saturating_duration_since(Instant::now()))
                .await
            {
                Response::Replied(()) => {}
                Response::Exited => {
                    warn_unlimited!("device {name} has exited; resuming without it");
                }
                Response::TimedOut => {
                    warn_unlimited!("device {name} did not resume in time");
                }
            }
        }
    }
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
        let (quiesced, blocks) = self.parts.quiesce_devices().await;
        Ok(PausedMachine {
            parts: self.parts,
            quiesced,
            blocks,
        })
    }

    /// Stops the vCPUs and the devices, closes the VM and hands its memory
    /// back; see [`PausedMachine::teardown`].
    pub async fn teardown(self) -> Result<MemoryStore, TeardownError> {
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
    pub async fn teardown(self) -> Result<MemoryStore, TeardownError> {
        self.parts.teardown().await
    }
}

/// A machine whose vCPUs are parked and whose devices are quiesced, as far as
/// they answered.
#[derive(Debug)]
pub struct PausedMachine {
    parts: Box<Parts>,
    quiesced: Vec<QuiescedDevice>,
    blocks: BTreeMap<String, QuiescedBlock>,
}

impl PausedMachine {
    /// A machine just built: vCPU threads and device tasks all started parked.
    pub(super) fn built(parts: Box<Parts>) -> Self {
        let quiesced = parts
            .platform
            .devices()
            .iter()
            .map(|device| QuiescedDevice {
                name: device.name().into(),
                response: Response::Replied(()),
            })
            .collect();
        // Cold assembly alone uses this: no guest has had access to an activator.
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
            blocks,
        }
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

    /// Typed block outcomes, keyed by the normalized configuration ID.
    pub fn quiesced_blocks(&self) -> &BTreeMap<String, QuiescedBlock> {
        &self.blocks
    }

    /// Stops the vCPUs and then the devices, closes the VM and hands its
    /// memory back.
    ///
    /// Each stage waits a bounded time and the later stages run whatever the
    /// earlier ones did. The memory comes back only if every vCPU thread has
    /// ended and every device has stopped; otherwise it stays mapped for
    /// whatever still runs and is released once that ends.
    pub async fn teardown(self) -> Result<MemoryStore, TeardownError> {
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
