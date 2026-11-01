//! A built machine: running ([`Machine`]), paused ([`PausedMachine`]), or
//! teardown-only after a transition failure ([`FailedMachine`]).
//!
//! Transitions consume their source state. Only acknowledged CPU transitions
//! with live memory dependencies produce a usable destination state.
//! They are not meant to be cancelled:
//! every wait inside them is bounded by a timeout, so they always finish.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use logger::warn_unlimited;
use tokio::time::Instant;

use super::block::AttachedBlock;
use super::cleanup::PendingCleanup;
use super::mem::{AttachedMem, MachineResizeError};
use super::plan::MachinePlan;
use super::platform::{Platform, PlatformQuiesced};
use super::vcpu::{VcpuError, Vcpus};
use crate::devices::runtime::{DeviceRuntime, Response, ShutdownError};
use crate::devices::virtio::block::BlockQuiesced;
use crate::devices::virtio::mem::{MemQuiesced, ResizeReply};
use crate::memory::layout::WindowId;
use crate::memory::service::{MemoryHandle, MemoryQuiesced, MemoryReleaseError};
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

/// Why a paused machine could not safely resume its memory dependencies or CPUs.
#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum ResumeError {
    /// resume the vCPUs
    Vcpus(#[source] VcpuError),
    /// memory service for window {0} did not resume in time
    MemoryServiceTimedOut(WindowId),
    /// memory service for window {0} exited before the vCPUs could resume
    MemoryServiceExited(WindowId),
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum TeardownError {
    /// stop the vCPUs
    Vcpus(#[source] VcpuError),
    /// memory service for window {0} did not release its registrations in time
    MemoryServiceTimedOut(WindowId),
    /// memory service for window {0} exited without proving registration cleanup
    MemoryServiceExited(WindowId),
    /// release unstarted memory resources for window {window}
    MemoryRelease {
        window: WindowId,
        #[source]
        source: MemoryReleaseError,
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuiescedMem {
    pub worker: Response<MemQuiesced>,
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
    pub(super) mems: BTreeMap<String, AttachedMem>,
    pub(super) memory_services: BTreeMap<WindowId, MemoryHandle>,
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
    ) -> (
        PlatformQuiesced,
        BTreeMap<String, QuiescedBlock>,
        BTreeMap<String, QuiescedMem>,
        BTreeMap<WindowId, Response<MemoryQuiesced>>,
    ) {
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
        let mut mem_workers = BTreeMap::new();
        for (id, mem) in &self.mems {
            let response = mem
                .worker
                .quiesce(deadline.saturating_duration_since(Instant::now()))
                .await;
            warn_response(mem.worker.name(), &response, "quiesce");
            mem_workers.insert(id.clone(), response);
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
        let mut mems = BTreeMap::new();
        for (id, mem) in &self.mems {
            let resample = mem
                .resample
                .quiesce(deadline.saturating_duration_since(Instant::now()))
                .await;
            warn_response(mem.resample.name(), &resample, "quiesce");
            mems.insert(
                id.clone(),
                QuiescedMem {
                    worker: mem_workers[id],
                    resample,
                },
            );
        }
        let mut memory_services = BTreeMap::new();
        for (window, service) in &self.memory_services {
            let response = service
                .quiesce(deadline.saturating_duration_since(Instant::now()))
                .await;
            warn_response(&format!("memory-service-{window}"), &response, "quiesce");
            memory_services.insert(*window, response);
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
        (platform, blocks, mems, memory_services)
    }

    fn failed_memory_service(&self) -> Option<WindowId> {
        self.memory_services
            .iter()
            .find_map(|(window, service)| service.has_exited().then_some(*window))
    }

    /// Resumes memory services first, within the devices' shared deadline.
    /// Their acknowledgements and live handles are required before any device
    /// resumes: they exclusively own the guest's hotplug RAM registrations.
    /// Other device failures remain local.
    async fn resume_devices(&self) -> Result<(), ResumeError> {
        let deadline = Instant::now() + DEVICE_TIMEOUT;
        if let Some(window) = self.failed_memory_service() {
            return Err(ResumeError::MemoryServiceExited(window));
        }
        for (window, service) in &self.memory_services {
            if service.has_exited() {
                return Err(ResumeError::MemoryServiceExited(*window));
            }
            let response = service
                .resume(deadline.saturating_duration_since(Instant::now()))
                .await;
            memory_resume_response(*window, response, service.has_exited())?;
        }
        // An earlier service may have exited while another acknowledged.
        if let Some(window) = self.failed_memory_service() {
            return Err(ResumeError::MemoryServiceExited(window));
        }
        for block in self.blocks.values() {
            let response = block
                .resample
                .resume(deadline.saturating_duration_since(Instant::now()))
                .await;
            warn_response(block.resample.name(), &response, "resume");
        }
        for mem in self.mems.values() {
            let response = mem
                .resample
                .resume(deadline.saturating_duration_since(Instant::now()))
                .await;
            warn_response(mem.resample.name(), &response, "resume");
        }
        for block in self.blocks.values() {
            let response = block
                .worker
                .resume(deadline.saturating_duration_since(Instant::now()))
                .await;
            warn_response(block.worker.name(), &response, "resume");
        }
        for mem in self.mems.values() {
            let response = mem
                .worker
                .resume(deadline.saturating_duration_since(Instant::now()))
                .await;
            warn_response(mem.worker.name(), &response, "resume");
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
        Ok(())
    }
}

/// A queued acknowledgement cannot prove safety after the service has exited.
fn memory_resume_response(
    window: WindowId,
    response: Response<()>,
    has_exited: bool,
) -> Result<(), ResumeError> {
    match response {
        Response::Replied(()) if !has_exited => Ok(()),
        Response::TimedOut if !has_exited => Err(ResumeError::MemoryServiceTimedOut(window)),
        Response::Replied(()) | Response::Exited | Response::TimedOut => {
            Err(ResumeError::MemoryServiceExited(window))
        }
    }
}

fn erased<Q>(response: &Response<Q>) -> Response<()> {
    match response {
        Response::Replied(_) => Response::Replied(()),
        Response::Exited => Response::Exited,
        Response::TimedOut => Response::TimedOut,
    }
}

fn platform_diagnostics(platform: &PlatformQuiesced) -> Vec<QuiescedDevice> {
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

fn quiesced_diagnostics(
    platform: &PlatformQuiesced,
    mems: &BTreeMap<String, QuiescedMem>,
    memory_services: &BTreeMap<WindowId, Response<MemoryQuiesced>>,
) -> Vec<QuiescedDevice> {
    let mut devices = platform_diagnostics(platform);
    for (id, mem) in mems {
        devices.push(QuiescedDevice {
            name: id.clone().into(),
            response: erased(&mem.worker),
        });
        devices.push(QuiescedDevice {
            name: format!("irq-{id}").into(),
            response: mem.resample,
        });
    }
    for (window, response) in memory_services {
        devices.push(QuiescedDevice {
            name: format!("memory-service-{window}").into(),
            response: erased(response),
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use api::RawVmmOp;
    use api::boot::{KernelConfig, SetKernel};
    use api::machine::{
        ConsoleConfig, HotplugSizeMib, MemoryHotplugConfig, SetConsole, SetMemoryHotplug,
    };
    use tokio::sync::mpsc;

    use super::*;
    use crate::devices::control::Command;
    use crate::devices::runtime::{Placement, Start};
    use crate::devices::virtio::mem::ResizeError;
    use crate::kvm_context::KvmContext;
    use crate::machine::build::build_with;
    use crate::machine::platform::{Captured, ConsoleBackend};
    use crate::memory::service::MemoryPhase;
    use crate::spec::{MachineSpec, VIRTIO_MEM_ID};
    use crate::test_guest::{self, TempFile};

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn hotplug_spec(kernel: &TempFile) -> MachineSpec {
        MachineSpec::default()
            .apply(
                SetKernel(KernelConfig {
                    path: kernel.path().into(),
                    cmdline: None,
                    initrd: None,
                })
                .into_vmm_command(),
            )
            .unwrap()
            .apply(SetConsole(ConsoleConfig::None).into_vmm_command())
            .unwrap()
            .apply(
                SetMemoryHotplug(MemoryHotplugConfig {
                    size_mib: 128,
                    block_size_mib: None,
                    chunk_size_mib: None,
                })
                .into_vmm_command(),
            )
            .unwrap()
    }

    #[test]
    fn memory_resume_requires_both_an_acknowledgement_and_a_live_service() {
        use std::error::Error;

        let window = WindowId(73);
        for has_exited in [false, true] {
            for response in [Response::Replied(()), Response::Exited, Response::TimedOut] {
                let result = memory_resume_response(window, response, has_exited);
                match (response, has_exited) {
                    (Response::Replied(()), false) => assert!(result.is_ok()),
                    (Response::TimedOut, false) => {
                        let error = result.unwrap_err();
                        assert!(
                            matches!(error, ResumeError::MemoryServiceTimedOut(id) if id == window)
                        );
                        assert_eq!(
                            error.to_string(),
                            "memory service for window 73 did not resume in time"
                        );
                        assert!(error.source().is_none());
                    }
                    _ => {
                        let error = result.unwrap_err();
                        assert!(
                            matches!(error, ResumeError::MemoryServiceExited(id) if id == window)
                        );
                        assert_eq!(
                            error.to_string(),
                            "memory service for window 73 exited before the vCPUs could resume"
                        );
                        assert!(error.source().is_none());
                    }
                }
            }
        }
        let error = ResumeError::Vcpus(VcpuError::TimedOut(0));
        assert!(matches!(
            error.source().unwrap().downcast_ref::<VcpuError>(),
            Some(VcpuError::TimedOut(0))
        ));
    }

    #[tokio::test(flavor = "local")]
    async fn failed_memory_resume_retains_the_same_parts_without_a_cpu_command() {
        let Some(ctx) = Path::new("/dev/kvm")
            .exists()
            .then(|| KvmContext::from_dev_path("/dev/kvm").unwrap())
        else {
            return;
        };
        let kernel = TempFile::new(&test_guest::image(&[0xeb, 0xfe], true));
        let spec = hotplug_spec(&kernel);
        for exited in [true, false] {
            let (plan, policy) = MachinePlan::new(&spec, ctx.capabilities()).unwrap();
            let window = plan.memory_hotplug().unwrap().window_id();
            let (mut paused, _events) = build_with(
                plan,
                policy,
                &ctx,
                ConsoleBackend::Captured(Captured::default()),
            )
            .unwrap();
            assert_eq!(paused.failed_memory_service(), None);
            let original_parts = std::ptr::from_ref(&*paused.parts);
            let mut blocker = None;
            let mut release = None;
            if exited {
                // Deliberately terminate the RAM owner while the CPUs are parked,
                // not as part of teardown. Stale quiesce diagnostics cannot hide it.
                let service = &paused.parts.memory_services[&window];
                service.stop();
                service.finished().await;
                assert_eq!(paused.failed_memory_service(), Some(window));
            } else {
                // An explicit gate on the I/O thread prevents any service ACK,
                // without depending on scheduling delays or a timing sleep.
                let (entered, wait_entered) = tokio::sync::oneshot::channel();
                let (sender, blocked) = std::sync::mpsc::channel();
                blocker = Some(
                    paused
                        .parts
                        .runtime
                        .spawn_on::<(), _, _, std::io::Error>(
                            "memory-resume-blocker",
                            Placement::Io,
                            Start::Running,
                            move |_control| async move {
                                entered.send(()).unwrap();
                                blocked.recv().unwrap();
                                Ok(())
                            },
                        )
                        .unwrap(),
                );
                release = Some(sender);
                wait_entered.await.unwrap();
            }
            let (failed, error) = paused.resume().await.unwrap_err();
            if exited {
                assert!(matches!(error, ResumeError::MemoryServiceExited(id) if id == window));
            } else {
                assert!(matches!(error, ResumeError::MemoryServiceTimedOut(id) if id == window));
            }
            assert_eq!(std::ptr::from_ref(&*failed.parts), original_parts);
            // Queuing even one CPU command invalidates capture's paused proof.
            // It remains valid here, so no resume or compensating pause was sent.
            assert_eq!(
                failed
                    .parts
                    .vcpus
                    .capture_state(failed.parts.vm.fd())
                    .unwrap()
                    .len(),
                1
            );
            if let Some(release) = release {
                release.send(()).unwrap();
                blocker.as_ref().unwrap().finished().await;
            }
            drop(blocker);
            failed.teardown().await.unwrap();
        }
    }

    #[tokio::test(flavor = "local")]
    async fn exited_workers_remain_local_even_when_a_block_id_matches_the_service_name() {
        use api::block::{AddDisk, BlockDevConfig};

        let Some(ctx) = Path::new("/dev/kvm")
            .exists()
            .then(|| KvmContext::from_dev_path("/dev/kvm").unwrap())
        else {
            return;
        };
        let kernel = TempFile::new(&test_guest::image(&[0xeb, 0xfe], true));
        let disk = TempFile::new(&[0; 512]);
        let spec = hotplug_spec(&kernel)
            .apply(
                AddDisk(BlockDevConfig {
                    id: Some("memory-service".into()),
                    path: disk.path().into(),
                    read_only: true,
                    root: false,
                })
                .into_vmm_command(),
            )
            .unwrap();
        let (plan, policy) = MachinePlan::new(&spec, ctx.capabilities()).unwrap();
        let (paused, _events) = build_with(
            plan,
            policy,
            &ctx,
            ConsoleBackend::Captured(Captured::default()),
        )
        .unwrap();
        let block = &paused.parts.blocks["memory-service"];
        assert_eq!(block.worker.name(), "memory-service");
        block.worker.stop();
        block.worker.finished().await;
        block.resample.stop();
        block.resample.finished().await;
        let mem = &paused.parts.mems[VIRTIO_MEM_ID];
        mem.worker.stop();
        mem.worker.finished().await;
        mem.resample.stop();
        mem.resample.finished().await;
        assert_eq!(paused.failed_memory_service(), None);
        let running = paused.resume().await.map_err(|(_, error)| error).unwrap();
        assert_eq!(running.failed_memory_service(), None);
        let paused = running.pause().await.map_err(|(_, error)| error).unwrap();
        assert_eq!(paused.failed_memory_service(), None);
        let running = paused.resume().await.map_err(|(_, error)| error).unwrap();
        running.teardown().await.unwrap();
    }

    #[tokio::test(flavor = "local")]
    async fn memory_lifecycle_acknowledges_devices_before_services_and_resumes_services_first() {
        let Some(ctx) = Path::new("/dev/kvm")
            .exists()
            .then(|| KvmContext::from_dev_path("/dev/kvm").unwrap())
        else {
            return;
        };
        let kernel = TempFile::new(&test_guest::image(&[0xeb, 0xfe], true));
        let spec = hotplug_spec(&kernel);
        let (plan, policy) = MachinePlan::new(&spec, ctx.capabilities()).unwrap();
        let window = plan.memory_hotplug().unwrap().window_id();
        let (mut paused, _events) = build_with(
            plan,
            policy,
            &ctx,
            ConsoleBackend::Captured(Captured::default()),
        )
        .unwrap();
        let status = paused.parts.mems[VIRTIO_MEM_ID].status.clone();
        let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
        let resample = paused
            .parts
            .runtime
            .spawn(
                "order-observer",
                Start::Running,
                move |mut control| async move {
                    loop {
                        match control.recv().await {
                            Command::Resume(ack) => {
                                assert!(matches!(status.snapshot().phase, MemoryPhase::Running));
                                observed_tx.send("resume").unwrap();
                                ack.send(());
                            }
                            Command::Quiesce(ack) => {
                                match status.snapshot().phase {
                                    MemoryPhase::Running => observed_tx.send("quiesce").unwrap(),
                                    MemoryPhase::Quiesced => {}
                                    phase => panic!("unexpected memory phase: {phase:?}"),
                                }
                                ack.send(());
                            }
                            Command::Stop => return Ok::<(), std::io::Error>(()),
                        }
                    }
                },
            )
            .unwrap();
        paused.parts.mems.get_mut(VIRTIO_MEM_ID).unwrap().resample = resample;
        let mut running = paused.resume().await.map_err(|(_, error)| error).unwrap();
        assert_eq!(observed_rx.recv().await.unwrap(), "resume");
        for size in [2, 0, 128, 4] {
            tokio::time::timeout(
                TIMEOUT,
                running.resize_memory(HotplugSizeMib(size)).unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        }
        for size in [1, 130, u32::MAX] {
            assert!(matches!(
                tokio::time::timeout(
                    TIMEOUT,
                    running.resize_memory(HotplugSizeMib(size)).unwrap()
                )
                .await
                .unwrap(),
                Err(ResizeError::Config(_))
            ));
        }
        let paused = running.pause().await.map_err(|(_, error)| error).unwrap();
        assert_eq!(observed_rx.recv().await.unwrap(), "quiesce");
        let Response::Replied(mem) = paused.quiesced_mems()[VIRTIO_MEM_ID].worker else {
            panic!("memory worker did not acknowledge quiesce")
        };
        assert_eq!(mem.requested_size, 4 << 20);
        assert_eq!(mem.usable_region_size, 128 << 20);
        assert_eq!(
            paused.quiesced_mems()[VIRTIO_MEM_ID].resample,
            Response::Replied(())
        );
        let Response::Replied(service) = &paused.quiesced_memory_services()[&window] else {
            panic!("memory service did not acknowledge after its worker")
        };
        assert_eq!(service.population.plugged_size(), 0);
        assert_eq!(service.revision, mem.memory_revision);
        let snapshot = paused.capture().await.unwrap();
        assert_eq!(snapshot.memory[&window].population, *service.population);
        assert_eq!(snapshot.memory[&window].revision, service.revision);
        let super::super::snapshot::DeviceSnapshot::Mem(saved) = &snapshot.devices[VIRTIO_MEM_ID]
        else {
            panic!("memory snapshot has the wrong device kind")
        };
        assert_eq!(saved.state, mem);
        let running = paused.resume().await.map_err(|(_, error)| error).unwrap();
        assert_eq!(observed_rx.recv().await.unwrap(), "resume");
        running.teardown().await.unwrap();
    }

    #[test]
    fn diagnostics_preserve_unproven_mem_and_service_outcomes() {
        let platform = PlatformQuiesced {
            serial: None,
            output: None,
        };
        let mems = BTreeMap::from([(
            "mem".into(),
            QuiescedMem {
                worker: Response::TimedOut,
                resample: Response::Exited,
            },
        )]);
        let services = BTreeMap::from([(WindowId(73), Response::TimedOut)]);
        assert_eq!(
            quiesced_diagnostics(&platform, &mems, &services),
            vec![
                QuiescedDevice {
                    name: "mem".into(),
                    response: Response::TimedOut
                },
                QuiescedDevice {
                    name: "irq-mem".into(),
                    response: Response::Exited
                },
                QuiescedDevice {
                    name: "memory-service-73".into(),
                    response: Response::TimedOut
                },
            ]
        );
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
    /// Query the retained service capabilities, never diagnostic task names.
    pub(crate) fn failed_memory_service(&self) -> Option<WindowId> {
        self.parts.failed_memory_service()
    }

    pub fn resize_memory(
        &mut self,
        size: api::machine::HotplugSizeMib,
    ) -> Result<ResizeReply, MachineResizeError> {
        let mem = self
            .parts
            .mems
            .get_mut(crate::spec::VIRTIO_MEM_ID)
            .ok_or(MachineResizeError::NotConfigured)?;
        mem.resize
            .send(u64::from(size.0) * (1 << 20))
            .map_err(MachineResizeError::Resize)
    }

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
        let (platform, blocks, mems, memory_services) = self.parts.quiesce_devices().await;
        Ok(PausedMachine {
            parts: self.parts,
            quiesced: quiesced_diagnostics(&platform, &mems, &memory_services),
            platform,
            blocks,
            mems,
            memory_services,
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

/// A teardown-only machine after a CPU or memory dependency transition failed.
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
    mems: BTreeMap<String, QuiescedMem>,
    memory_services: BTreeMap<WindowId, Response<MemoryQuiesced>>,
}

impl PausedMachine {
    /// A machine just built: vCPU threads and device tasks all started parked.
    pub(super) fn built(parts: Box<Parts>, platform: PlatformQuiesced) -> Self {
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
        // Cold tasks are parked, but their first typed acknowledgement has not been observed.
        let mems = parts
            .mems
            .keys()
            .map(|id| {
                (
                    id.clone(),
                    QuiescedMem {
                        worker: Response::TimedOut,
                        resample: Response::TimedOut,
                    },
                )
            })
            .collect();
        let memory_services = parts
            .memory_services
            .keys()
            .map(|window| (*window, Response::TimedOut))
            .collect();
        let quiesced = quiesced_diagnostics(&platform, &mems, &memory_services);
        Self {
            parts,
            quiesced,
            platform,
            blocks,
            mems,
            memory_services,
        }
    }

    pub(super) fn record_quiesced(
        &mut self,
        platform: PlatformQuiesced,
        blocks: BTreeMap<String, QuiescedBlock>,
        mems: BTreeMap<String, QuiescedMem>,
        memory_services: BTreeMap<WindowId, Response<MemoryQuiesced>>,
    ) {
        self.quiesced = quiesced_diagnostics(&platform, &mems, &memory_services);
        self.platform = platform;
        self.blocks = blocks;
        self.mems = mems;
        self.memory_services = memory_services;
    }

    pub(super) fn into_cleanup(self) -> PendingCleanup {
        PendingCleanup::from_parts(self.parts)
    }

    /// Query the retained service capabilities, not the quiesce diagnostics.
    pub(crate) fn failed_memory_service(&self) -> Option<WindowId> {
        self.parts.failed_memory_service()
    }

    /// Resumes live, acknowledged memory services, then devices and vCPUs.
    ///
    /// Other device failures remain local. A memory dependency or CPU failure
    /// returns teardown-only ownership, never a restored paused state.
    pub async fn resume(self) -> Result<Machine, (FailedMachine, ResumeError)> {
        // Consume the paused proof before any resource is resumed.
        let Self { mut parts, .. } = self;
        if let Err(error) = parts.resume_devices().await {
            return Err((FailedMachine { parts }, error));
        }
        // A service may have exited while devices resumed. This is not a lock
        // against later exits; the main loop keeps observing the live handles.
        if let Some(window) = parts.failed_memory_service() {
            return Err((
                FailedMachine { parts },
                ResumeError::MemoryServiceExited(window),
            ));
        }
        if let Err(error) = parts.vcpus.resume(VCPU_TIMEOUT).await {
            return Err((FailedMachine { parts }, ResumeError::Vcpus(error)));
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

    pub fn quiesced_mems(&self) -> &BTreeMap<String, QuiescedMem> {
        &self.mems
    }

    pub fn quiesced_memory_services(&self) -> &BTreeMap<WindowId, Response<MemoryQuiesced>> {
        &self.memory_services
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
