//! A built machine, either running ([`Machine`]) or paused
//! ([`PausedMachine`]).
//!
//! Transitions take the machine by value and hand it back in its new state,
//! or in its old one beside the error. They are not meant to be cancelled:
//! every wait inside them is bounded by a timeout, so they always finish.

use std::sync::Arc;
use std::time::Duration;

use logger::warn_unlimited;
use tokio::time::Instant;

use super::plan::MachinePlan;
use super::platform::Platform;
use super::vcpu::{VcpuError, Vcpus};
use crate::devices::runtime::{DeviceRuntime, Response, ShutdownError};
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

/// Everything a built machine owns, whatever its state. Fields drop in order,
/// so the vCPUs are told to stop before anything they reach goes away.
#[derive(Debug)]
pub(super) struct Parts {
    pub(super) vcpus: Vcpus,
    pub(super) runtime: DeviceRuntime,
    pub(super) platform: Platform,
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
            bindings,
            memslots,
            vm,
            plan: _,
        } = self;
        let vcpus = vcpus.stop(VCPU_TIMEOUT).await;
        let devices = runtime.shutdown(DEVICE_GRACE).await;
        // Restores the terminal, now that nothing serves the console.
        drop(platform);
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
    async fn quiesce_devices(&self) -> Vec<QuiescedDevice> {
        let deadline = Instant::now() + DEVICE_TIMEOUT;
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
        quiesced
    }

    /// Asks every device to resume, within one shared deadline. A device that
    /// has exited or does not answer is left as it is.
    async fn resume_devices(&self) {
        let deadline = Instant::now() + DEVICE_TIMEOUT;
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
    /// Fails only if a vCPU has exited or does not acknowledge in time. The
    /// machine then comes back running: every vCPU is sent `Resume`, which a
    /// vCPU that has not yet seen its `Pause` sees right after it, and the
    /// devices were never quiesced.
    ///
    /// A device that has exited or does not answer does not fail the pause:
    /// it is recorded in [`PausedMachine::quiesced`] and the pause completes.
    pub async fn pause(mut self) -> Result<PausedMachine, (Self, PauseError)> {
        if let Err(error) = self.parts.vcpus.pause(VCPU_TIMEOUT).await {
            if let Err(again) = self.parts.vcpus.resume(VCPU_TIMEOUT).await {
                warn_unlimited!("after a failed pause: {}", render_chain(&again));
            }
            return Err((self, PauseError(error)));
        }
        let quiesced = self.parts.quiesce_devices().await;
        Ok(PausedMachine {
            parts: self.parts,
            quiesced,
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

/// A machine whose vCPUs are parked and whose devices are quiesced, as far as
/// they answered.
#[derive(Debug)]
pub struct PausedMachine {
    parts: Box<Parts>,
    quiesced: Vec<QuiescedDevice>,
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
        Self { parts, quiesced }
    }

    /// Resumes the devices, then the vCPUs.
    ///
    /// A device that has exited or does not answer is left as it is. Fails
    /// only if a vCPU has exited or does not acknowledge in time; the machine
    /// then comes back paused: the vCPUs are paused again, and the devices
    /// quiesced again, with what they answered recorded anew. A vCPU that
    /// does not acknowledge that pause either still has it queued and kicked,
    /// so it runs no guest code past the exit it is handling.
    pub async fn resume(mut self) -> Result<Machine, (Self, ResumeError)> {
        self.parts.resume_devices().await;
        if let Err(error) = self.parts.vcpus.resume(VCPU_TIMEOUT).await {
            if let Err(again) = self.parts.vcpus.pause(VCPU_TIMEOUT).await {
                warn_unlimited!("after a failed resume: {}", render_chain(&again));
            }
            self.quiesced = self.parts.quiesce_devices().await;
            return Err((self, ResumeError(error)));
        }
        Ok(Machine { parts: self.parts })
    }

    /// What each device answered when the machine paused, in attach order.
    pub fn quiesced(&self) -> &[QuiescedDevice] {
        &self.quiesced
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
