use std::io;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use virtio_queue::{Queue, QueueT};

use kvm_ioctls::IoEventAddress;
use vmm_sys_util::eventfd::EventFd;

use super::block::MmioDevices;
use super::cleanup::CleanupParts;
use super::plan::MemPlan;
use super::snapshot::{CaptureMemoryError, MemorySnapshot};
use super::state::QuiescedMem;
use crate::bus::BusDevice;
use crate::devices::runtime::{DeviceHandle, Placement, Response, SpawnError, Start};
use crate::devices::virtio::mem::{
    MemError, MemHandle, MemQuiesced, MemResources, MemRestore, ResizeError, ResizeHandle, protocol,
};
use crate::devices::virtio::mmio::{
    MmioTransport, MmioTransportSnapshot, NOTIFY_OFFSET, RestoreError, TransportError,
};
use crate::devices::virtio::{Activation, ActivationError, IrqTrigger, session_channel};
use crate::memory::hotplug::Population;
use crate::memory::layout::WindowId;
use crate::memory::service::{
    MemoryHandle, MemoryQuiesced, MemoryResources, MemoryService, ServiceSetupError, WindowStatus,
};
use crate::memory::store::MemoryView;
use crate::resources::IdPoolError;
use crate::space::{Gpa, MemSlot};
use crate::vm::{KvmVm, SlotPermit};

const DRIVER_OK: u32 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemSnapshot {
    pub transport: MmioTransportSnapshot,
    pub state: MemQuiesced,
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum RestoreMemError {
    /// validate the saved memory device state and configuration
    State(#[source] MemError),
    /// validate the saved memory transport
    Transport(#[from] RestoreError),
    /// saved memory backend queue does not match the transport
    QueueTransport,
    /// validate the saved memory backend queue
    Queue(#[source] MemError),
}

impl MemSnapshot {
    pub(super) fn validate(
        &self,
        plan: &MemPlan,
        memory: &MemorySnapshot,
        view: &MemoryView,
    ) -> Result<Option<Queue>, RestoreMemError> {
        MemResources::validate_restore(
            plan.geometry(),
            &memory.population,
            memory.revision,
            &MemRestore {
                state: self.state,
                config: self.transport.config.clone(),
            },
        )
        .map_err(RestoreMemError::State)?;
        let desc = MemResources::descriptor(plan.geometry());
        self.transport.validate(&desc, view)?;
        let Some(state) = self.state.queue else {
            return Ok(None);
        };
        // An owned backend queue can consume buffers on resume, which requires DRIVER_OK.
        if self.transport.regs.status & DRIVER_OK == 0 {
            return Err(RestoreMemError::QueueTransport);
        }
        protocol::validate_features(self.transport.regs.driver_features)
            .map_err(MemError::Features)
            .map_err(RestoreMemError::Queue)?;
        let [regs] = self.transport.regs.queues.as_slice() else {
            return Err(RestoreMemError::QueueTransport);
        };
        if state.max_size != protocol::QUEUE_MAX
            || u32::from(state.size) != regs.size
            || !state.ready
            || state.ready != regs.ready
            || state.desc_table != regs.desc.0
            || state.avail_ring != regs.avail.0
            || state.used_ring != regs.used.0
            || state.event_idx_enabled
        {
            return Err(RestoreMemError::QueueTransport);
        }
        let queue = Queue::try_from(state)
            .map_err(MemError::Queue)
            .map_err(RestoreMemError::Queue)?;
        if !queue.is_valid(view.as_ref()) {
            return Err(RestoreMemError::Queue(MemError::QueueConfiguration));
        }
        let available = queue
            .avail_idx(view.as_ref(), Ordering::Acquire)
            .map_err(MemError::Queue)
            .map_err(RestoreMemError::Queue)?
            .0;
        let used = queue
            .used_idx(view.as_ref(), Ordering::Acquire)
            .map_err(MemError::Queue)
            .map_err(RestoreMemError::Queue)?
            .0;
        if state.next_avail != state.next_used || used != state.next_used {
            return Err(RestoreMemError::Queue(MemError::QueuePosition));
        }
        if available.wrapping_sub(state.next_avail) > state.size {
            return Err(RestoreMemError::Queue(MemError::AvailableIndex));
        }
        Ok(Some(queue))
    }
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum CaptureMemError {
    /// memory worker exited before capture
    WorkerExited,
    /// memory worker did not quiesce in time
    WorkerTimedOut,
    /// memory interrupt resampler exited before capture
    ResampleExited,
    /// memory interrupt resampler did not quiesce in time
    ResampleTimedOut,
    /// validate the quiesced memory device
    State(#[from] RestoreMemError),
    /// memory device state changed while KVM state was captured
    Changed,
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum MachineResizeError {
    /// no memory hotplug device is configured
    NotConfigured,
    /// enqueue the memory hotplug target
    Resize(#[from] ResizeError),
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum AttachMemError {
    /// reserve chunk slots for memory device {id}
    Slots {
        id: String,
        #[source]
        source: IdPoolError<MemSlot>,
    },
    /// prepare the service for memory device {id}
    Service {
        id: String,
        #[source]
        source: ServiceSetupError,
    },
    /// prepare memory device {id}
    Resources {
        id: String,
        #[source]
        source: MemError,
    },
    /// duplicate the queue notifier for memory device {id}
    Notifier {
        id: String,
        #[source]
        source: io::Error,
    },
    /// construct the transport for memory device {id}
    Transport {
        id: String,
        #[source]
        source: TransportError,
    },
    /// validate the restored state for memory device {id}
    State {
        id: String,
        #[source]
        source: RestoreMemError,
    },
    /// restore the transport for memory device {id}
    Restore {
        id: String,
        #[source]
        source: RestoreError,
    },
    /// queue the restored session for memory device {id}
    Activate {
        id: String,
        #[source]
        source: ActivationError,
    },
    /// reassert the pending memory interrupt
    Interrupt(#[source] io::Error),
    /// register the queue notifier for memory device {id}
    Ioevent {
        id: String,
        #[source]
        source: kvm_ioctls::Error,
    },
    /// register the interrupt for memory device {id}
    Irqfd {
        id: String,
        #[source]
        source: kvm_ioctls::Error,
    },
    /// start tasks for memory device {id}
    Spawn {
        id: String,
        #[source]
        source: SpawnError,
    },
}

pub(super) struct PreparedMem {
    service: MemoryService,
    resources: MemResources,
    queue_notifier: EventFd,
    transport: Arc<MmioTransport>,
    interrupt: Arc<IrqTrigger>,
    resize: ResizeHandle,
    status: WindowStatus,
}

impl PreparedMem {
    pub(super) fn cold(
        id: &str,
        plan: &MemPlan,
        vm: Arc<KvmVm>,
        permits: Vec<SlotPermit>,
    ) -> Result<Self, AttachMemError> {
        let geometry = plan.geometry();
        let (service, window, status) =
            MemoryService::prepare(Arc::clone(&vm), Population::new(geometry), permits).map_err(
                |(resources, source)| {
                    // Empty population cannot have registered a hotplug chunk.
                    assert_eq!(resources.bound_chunks(), 0);
                    drop(resources);
                    AttachMemError::Service {
                        id: id.into(),
                        source,
                    }
                },
            )?;
        let (resources, handle) =
            MemResources::new(geometry, window, status.clone()).map_err(|source| {
                AttachMemError::Resources {
                    id: id.into(),
                    source,
                }
            })?;
        let MemHandle {
            activator,
            queue_notifier,
            interrupt,
            desc,
            config,
            resize,
        } = handle;
        let notifier = queue_notifier
            .try_clone()
            .map_err(|source| AttachMemError::Notifier {
                id: id.into(),
                source,
            })?;
        let transport = MmioTransport::new(
            desc,
            config,
            Arc::clone(vm.memory().view()),
            activator,
            Arc::clone(&interrupt),
            vec![notifier],
        )
        .map_err(|source| AttachMemError::Transport {
            id: id.into(),
            source,
        })?;
        Ok(Self {
            service,
            resources,
            queue_notifier,
            transport: Arc::new(transport),
            interrupt,
            resize,
            status,
        })
    }

    pub(super) fn restore(
        id: &str,
        plan: &MemPlan,
        vm: Arc<KvmVm>,
        permits: Vec<SlotPermit>,
        memory: MemorySnapshot,
        snapshot: MemSnapshot,
    ) -> Result<Self, (MemoryResources, AttachMemError)> {
        let (service, window, status) = MemoryService::prepare_restored(
            Arc::clone(&vm),
            memory.population.clone(),
            memory.revision,
            permits,
        )
        .map_err(|(resources, source)| {
            (
                resources,
                AttachMemError::Service {
                    id: id.into(),
                    source,
                },
            )
        })?;
        let prepared = (|| {
            let queue = snapshot
                .validate(plan, &memory, vm.memory().view())
                .map_err(|source| AttachMemError::State {
                    id: id.into(),
                    source,
                })?;
            let (resources, handle) = MemResources::restore(
                plan.geometry(),
                window,
                status.clone(),
                MemRestore {
                    state: snapshot.state,
                    config: snapshot.transport.config.clone(),
                },
            )
            .map_err(|source| AttachMemError::Resources {
                id: id.into(),
                source,
            })?;
            let MemHandle {
                activator,
                queue_notifier,
                interrupt,
                desc,
                config,
                resize,
            } = handle;
            let notifier =
                queue_notifier
                    .try_clone()
                    .map_err(|source| AttachMemError::Notifier {
                        id: id.into(),
                        source,
                    })?;
            let driver_features = snapshot.transport.regs.driver_features;
            let (control, session) = match queue.as_ref() {
                Some(_) => {
                    let (control, session) = session_channel();
                    (Some(control), Some(session))
                }
                None => (None, None),
            };
            let transport = MmioTransport::restore(
                desc,
                config,
                Arc::clone(vm.memory().view()),
                activator.clone(),
                Arc::clone(&interrupt),
                vec![notifier],
                snapshot.transport,
                control,
            )
            .map_err(|source| AttachMemError::Restore {
                id: id.into(),
                source,
            })?;
            if let Some(queue) = queue {
                activator
                    .activate(Activation {
                        queues: vec![queue],
                        mem: Arc::clone(vm.memory().view()),
                        interrupt: Arc::clone(&interrupt),
                        driver_features,
                        session: session.expect("restored backend owns its session"),
                    })
                    .map_err(|source| AttachMemError::Activate {
                        id: id.into(),
                        source,
                    })?;
            }
            Ok::<_, AttachMemError>((
                resources,
                queue_notifier,
                Arc::new(transport),
                interrupt,
                resize,
            ))
        })();
        match prepared {
            Ok((resources, queue_notifier, transport, interrupt, resize)) => Ok(Self {
                service,
                resources,
                queue_notifier,
                transport,
                interrupt,
                resize,
                status,
            }),
            Err(error) => Err((service.into_resources(), error)),
        }
    }

    pub(super) fn into_resources(self) -> (WindowId, MemoryResources) {
        let window = self.status.snapshot().population.geometry().window_id();
        (window, self.service.into_resources())
    }

    pub(super) fn attach(
        self,
        id: &str,
        plan: &MemPlan,
        parts: &mut CleanupParts,
        mmio: &mut MmioDevices,
    ) -> Result<AttachedMem, AttachMemError> {
        let Self {
            service,
            resources,
            queue_notifier,
            transport,
            interrupt,
            resize,
            status,
        } = self;
        let window = plan.window_id();
        assert!(!parts.memory_services.contains_key(&window));
        let handle =
            service
                .spawn(&mut parts.runtime, Start::Parked)
                .map_err(|(service, source)| {
                    let resources = service.into_resources();
                    assert!(parts.unstarted_memory.insert(window, resources).is_none());
                    AttachMemError::Spawn {
                        id: id.into(),
                        source,
                    }
                })?;
        parts.memory_services.insert(window, handle);
        parts.memory_statuses.insert(window, status.clone());

        let grant = plan.grant();
        parts
            .vm
            .fd()
            .register_ioevent(
                &queue_notifier,
                &IoEventAddress::Mmio(grant.mmio.start().0 + NOTIFY_OFFSET),
                0u32,
            )
            .map_err(|source| AttachMemError::Ioevent {
                id: id.into(),
                source,
            })?;
        parts
            .vm
            .fd()
            .register_irqfd_with_resample(interrupt.irq_fd(), interrupt.resample_fd(), grant.gsi.0)
            .map_err(|source| AttachMemError::Irqfd {
                id: id.into(),
                source,
            })?;
        let irq = Arc::clone(&interrupt);
        let resample = parts
            .runtime
            .spawn_on(
                format!("irq-{id}"),
                Placement::VmmMain,
                Start::Parked,
                move |control| irq.resample(control),
            )
            .map_err(|source| AttachMemError::Spawn {
                id: id.into(),
                source,
            })?;
        let worker = parts
            .runtime
            .spawn_on(
                id,
                Placement::Io,
                Start::Parked,
                move |control| async move { resources.into_device()?.run(control).await },
            )
            .map_err(|source| AttachMemError::Spawn {
                id: id.into(),
                source,
            })?;
        let attached = AttachedMem {
            worker,
            resample,
            transport,
            interrupt,
            resize,
            status,
        };
        mmio.push((
            grant.mmio,
            Arc::clone(&attached.transport) as Arc<dyn BusDevice<Gpa>>,
        ));
        Ok(attached)
    }
}

pub(super) struct AttachedMem {
    pub(super) worker: DeviceHandle<MemQuiesced>,
    pub(super) resample: DeviceHandle<()>,
    pub(super) transport: Arc<MmioTransport>,
    pub(super) interrupt: Arc<IrqTrigger>,
    pub(super) resize: ResizeHandle,
    pub(super) status: WindowStatus,
}

impl std::fmt::Debug for AttachedMem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AttachedMem")
            .field("worker", &self.worker)
            .field("resample", &self.resample)
            .field("status", &self.status)
            .field("interrupt_status", &self.interrupt.status())
            .finish_non_exhaustive()
    }
}

impl AttachedMem {
    pub(super) fn capture(
        &self,
        plan: &MemPlan,
        memory: &MemorySnapshot,
        view: &MemoryView,
        quiesced: &QuiescedMem,
    ) -> Result<MemSnapshot, CaptureMemError> {
        let state = match quiesced.worker {
            Response::Replied(state) => state,
            Response::Exited => return Err(CaptureMemError::WorkerExited),
            Response::TimedOut => return Err(CaptureMemError::WorkerTimedOut),
        };
        match quiesced.resample {
            Response::Replied(()) => {}
            Response::Exited => return Err(CaptureMemError::ResampleExited),
            Response::TimedOut => return Err(CaptureMemError::ResampleTimedOut),
        }
        self.check_alive()?;
        let snapshot = MemSnapshot {
            state,
            transport: self.transport.snapshot(),
        };
        snapshot.validate(plan, memory, view)?;
        self.check_alive()?;
        Ok(snapshot)
    }

    pub(super) fn capture_memory(
        &self,
        plan: &MemPlan,
        service: &MemoryHandle,
        quiesced: &Response<MemoryQuiesced>,
    ) -> Result<MemorySnapshot, CaptureMemoryError> {
        let ack = match quiesced {
            Response::Replied(ack) => ack,
            Response::Exited => return Err(CaptureMemoryError::ServiceExited),
            Response::TimedOut => return Err(CaptureMemoryError::ServiceTimedOut),
        };
        if service.has_exited() {
            return Err(CaptureMemoryError::ServiceExited);
        }
        let status = self.status.snapshot();
        if !matches!(status.phase, crate::memory::service::MemoryPhase::Quiesced) {
            return Err(CaptureMemoryError::NotQuiesced);
        }
        if status.population != ack.population || status.revision != ack.revision {
            return Err(CaptureMemoryError::Changed);
        }
        let snapshot = MemorySnapshot {
            population: (*ack.population).clone(),
            revision: ack.revision,
        };
        snapshot.validate(plan.geometry())?;
        if service.has_exited() {
            return Err(CaptureMemoryError::ServiceExited);
        }
        Ok(snapshot)
    }

    fn check_alive(&self) -> Result<(), CaptureMemError> {
        if self.worker.has_exited() {
            return Err(CaptureMemError::WorkerExited);
        }
        if self.resample.has_exited() {
            return Err(CaptureMemError::ResampleExited);
        }
        Ok(())
    }

    pub(super) fn reraise_pending(&self) -> Result<(), AttachMemError> {
        self.interrupt
            .reraise_pending()
            .map_err(AttachMemError::Interrupt)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::error::Error as _;
    use std::path::Path;
    use std::time::Duration;

    use api::RawVmmOp;
    use api::boot::{KernelConfig, SetKernel};
    use api::machine::{MemoryHotplugConfig, SetMemoryHotplug};
    use tokio::sync::oneshot;

    use super::*;
    use crate::devices::runtime::{DeviceRuntime, Response, ShutdownError};
    use crate::kvm_context::KvmContext;
    use crate::machine::cleanup::PendingCleanup;
    use crate::machine::plan::MachinePlan;
    use crate::machine::state::TeardownError;
    use crate::memory::policy::BackingKind;
    use crate::memory::store::MemoryStore;
    use crate::spec::{MachineSpec, VIRTIO_MEM_ID};
    use crate::vm::create_vm;

    const TIMEOUT: Duration = Duration::from_secs(5);

    #[test]
    fn backend_queue_requires_driver_ok_without_inferring_ownership_from_transport_status() {
        use vm_memory::{Bytes, GuestAddress};

        use crate::devices::virtio::mmio::{MmioRegs, QueueRegs};
        use crate::kvm_context::HostCapabilities;

        let host = HostCapabilities::fake(46, 32, 4096);
        let spec = MachineSpec::default()
            .apply(
                SetKernel(KernelConfig {
                    path: "/never-opened/kernel".into(),
                    cmdline: None,
                    initrd: None,
                })
                .into_vmm_command(),
            )
            .unwrap()
            .apply(
                SetMemoryHotplug(MemoryHotplugConfig {
                    size_mib: 128,
                    block_size_mib: None,
                    chunk_size_mib: None,
                })
                .into_vmm_command(),
            )
            .unwrap();
        let (plan, policy) = MachinePlan::new(&spec, &host).unwrap();
        let memory = MemoryStore::materialize(plan.memory().clone(), policy, &host).unwrap();
        let mem = plan.memory_hotplug().unwrap();
        let population = MemorySnapshot {
            population: Population::new(mem.geometry()),
            revision: 0,
        };
        let desc = MemResources::descriptor(mem.geometry());
        let mut snapshot = MemSnapshot {
            state: MemQuiesced {
                requested_size: 0,
                usable_region_size: 0,
                memory_revision: 0,
                queue: None,
            },
            transport: MmioTransportSnapshot {
                regs: MmioRegs::new(&desc),
                config: crate::devices::virtio::config::ConfigSnapshot {
                    bytes: desc.config.clone(),
                    generation: 0,
                },
                desc,
                interrupt_status: 0,
            },
        };
        snapshot.transport.regs.driver_features = protocol::REQUIRED_FEATURES;
        snapshot.transport.regs.queues = vec![QueueRegs {
            size: 8,
            ready: true,
            desc: Gpa(0x10000),
            avail: Gpa(0x11000),
            used: Gpa(0x12000),
        }];
        let queue = virtio_queue::QueueState {
            max_size: protocol::QUEUE_MAX,
            size: 8,
            ready: true,
            event_idx_enabled: false,
            next_avail: 0,
            next_used: 0,
            desc_table: 0x10000,
            avail_ring: 0x11000,
            used_ring: 0x12000,
        };
        memory.view().write_obj(1_u16, GuestAddress(0x11002)).unwrap();
        memory.view().write_obj(0_u16, GuestAddress(0x12002)).unwrap();
        for errors in [0, 64, 128, 64 | 128] {
            snapshot.transport.regs.status = 11 | errors;
            snapshot.state.queue = Some(queue);
            assert!(matches!(
                snapshot.validate(mem, &population, memory.view()),
                Err(RestoreMemError::QueueTransport)
            ));
            snapshot.state.queue = None;
            assert!(
                snapshot
                    .validate(mem, &population, memory.view())
                    .unwrap()
                    .is_none()
            );

            snapshot.transport.regs.status |= DRIVER_OK;
            snapshot.state.queue = Some(queue);
            assert!(
                snapshot
                    .validate(mem, &population, memory.view())
                    .unwrap()
                    .is_some()
            );
            snapshot.state.queue = None;
            assert!(
                snapshot
                    .validate(mem, &population, memory.view())
                    .unwrap()
                    .is_none()
            );
            snapshot.transport.regs.queues[0].ready = false;
            assert!(
                snapshot
                    .validate(mem, &population, memory.view())
                    .unwrap()
                    .is_none()
            );
            snapshot.transport.regs.queues[0].ready = true;
        }
        assert_eq!(memory.view().read_obj::<u16>(GuestAddress(0x11002)).unwrap(), 1);
        assert_eq!(memory.view().read_obj::<u16>(GuestAddress(0x12002)).unwrap(), 0);
    }

    #[tokio::test(flavor = "local")]
    async fn failed_ioevent_attachment_retains_the_started_service_and_ram_for_cleanup_retry() {
        let Some(ctx) = Path::new("/dev/kvm")
            .exists()
            .then(|| KvmContext::from_dev_path("/dev/kvm").unwrap())
        else {
            return;
        };
        let spec = MachineSpec::default()
            .apply(
                SetKernel(KernelConfig {
                    path: "/unused/kernel".into(),
                    cmdline: None,
                    initrd: None,
                })
                .into_vmm_command(),
            )
            .unwrap()
            .apply(
                SetMemoryHotplug(MemoryHotplugConfig {
                    size_mib: 128,
                    block_size_mib: None,
                    chunk_size_mib: None,
                })
                .into_vmm_command(),
            )
            .unwrap();
        for kind in [BackingKind::AnonymousPrivate, BackingKind::SharedMemfd] {
            let (plan, mut policy) = MachinePlan::new(&spec, ctx.capabilities()).unwrap();
            policy.kind = kind;
            let mem = plan.memory_hotplug().unwrap();
            let memory =
                MemoryStore::materialize(plan.memory().clone(), policy, ctx.capabilities())
                    .unwrap();
            let weak = Arc::downgrade(memory.view());
            let (vm, bindings, mut memslots) = create_vm(&ctx, memory).unwrap();
            let permits = memslots
                .allocate(mem.geometry().chunk_count() as usize)
                .unwrap();
            let prepared = PreparedMem::cold(VIRTIO_MEM_ID, mem, Arc::clone(&vm), permits).unwrap();
            let status = prepared.status.clone();
            assert_eq!(status.snapshot().population.plugged_size(), 0);
            assert_eq!(status.snapshot().revision, 0);
            vm.fd()
                .register_ioevent(
                    &prepared.queue_notifier,
                    &IoEventAddress::Mmio(mem.grant().mmio.start().0 + NOTIFY_OFFSET),
                    0u32,
                )
                .unwrap();
            let (mut runtime, _exits) = DeviceRuntime::new();
            let task_vm = Arc::clone(&vm);
            let (entered_tx, entered_rx) = oneshot::channel();
            let (release_tx, release_rx) = oneshot::channel();
            let gate: DeviceHandle<()> = runtime
                .spawn_on(
                    "cleanup-gate",
                    Placement::Io,
                    Start::Running,
                    move |control| async move {
                        entered_tx.send(()).unwrap();
                        release_rx.await.unwrap();
                        drop(task_vm);
                        drop(control);
                        Ok::<(), io::Error>(())
                    },
                )
                .unwrap();
            entered_rx.await.unwrap();
            let mut parts = Box::new(CleanupParts {
                vcpus: None,
                runtime,
                platform: None,
                blocks: BTreeMap::new(),
                mems: BTreeMap::new(),
                memory_services: BTreeMap::new(),
                memory_statuses: BTreeMap::new(),
                unstarted_memory: BTreeMap::new(),
                bindings,
                memslots,
                vm,
            });
            let error = prepared
                .attach(VIRTIO_MEM_ID, mem, &mut parts, &mut Vec::new())
                .unwrap_err();
            assert!(matches!(error, AttachMemError::Ioevent { .. }));
            assert!(parts.mems.is_empty());
            assert_eq!(parts.memory_services.len(), 1);
            let Response::Replied(ack) = parts.memory_services[&mem.window_id()]
                .quiesce(TIMEOUT)
                .await
            else {
                panic!("started service lost after attachment failure")
            };
            assert_eq!(ack.population.plugged_size(), 0);
            assert_eq!(ack.population.geometry(), mem.geometry());
            let mut pending = PendingCleanup::tasks(parts);
            for _ in 0..2 {
                let (owner, error) = pending.wait(Duration::ZERO).await.unwrap_err();
                assert!(matches!(
                    error,
                    TeardownError::MemoryServiceTimedOut(_)
                        | TeardownError::Devices(ShutdownError::Unresponsive(_))
                ));
                assert!(weak.upgrade().is_some());
                pending = owner;
            }
            release_tx.send(()).unwrap();
            let memory = pending.wait(TIMEOUT).await.unwrap();
            assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), memory.view()));
            drop(gate);
            drop(memory);
            assert!(weak.upgrade().is_none());
        }
    }

    fn restore_fixture(
        ctx: &KvmContext,
        kind: BackingKind,
    ) -> (
        MachinePlan,
        Arc<KvmVm>,
        Vec<crate::vm::Binding>,
        crate::vm::MemSlots,
        MemorySnapshot,
        MemSnapshot,
    ) {
        let spec = MachineSpec::default()
            .apply(
                SetKernel(KernelConfig {
                    path: "/unused/kernel".into(),
                    cmdline: None,
                    initrd: None,
                })
                .into_vmm_command(),
            )
            .unwrap()
            .apply(
                SetMemoryHotplug(MemoryHotplugConfig {
                    size_mib: 256,
                    block_size_mib: None,
                    chunk_size_mib: None,
                })
                .into_vmm_command(),
            )
            .unwrap();
        let (plan, mut policy) = MachinePlan::new(&spec, ctx.capabilities()).unwrap();
        policy.kind = kind;
        let memory =
            MemoryStore::materialize(plan.memory().clone(), policy, ctx.capabilities()).unwrap();
        let geometry = plan.memory_hotplug().unwrap().geometry();
        let population = Population::restore(
            geometry,
            [
                geometry.chunk(0).unwrap().placement(),
                geometry.chunk(1).unwrap().placement(),
            ],
        )
        .unwrap();
        let desc = MemResources::descriptor(geometry);
        let mut config = protocol::Config::decode(&desc.config).unwrap();
        config.plugged_size = population.plugged_size();
        config.usable_region_size = geometry.region().len();
        let snapshot = MemSnapshot {
            state: MemQuiesced {
                requested_size: 0,
                usable_region_size: config.usable_region_size,
                memory_revision: 7,
                queue: None,
            },
            transport: MmioTransportSnapshot {
                regs: crate::devices::virtio::mmio::MmioRegs::new(&desc),
                desc,
                config: crate::devices::virtio::config::ConfigSnapshot {
                    bytes: config.encode().to_vec(),
                    generation: 99,
                },
                interrupt_status: 3,
            },
        };
        let (vm, bindings, slots) = create_vm(ctx, memory).unwrap();
        (
            plan,
            vm,
            bindings,
            slots,
            MemorySnapshot {
                population,
                revision: 7,
            },
            snapshot,
        )
    }

    #[tokio::test(flavor = "local")]
    async fn partial_restored_bind_and_failed_release_keep_the_same_resources_and_ram_on_retry() {
        use vm_memory::{Bytes, GuestAddress};
        let Some(ctx) = Path::new("/dev/kvm")
            .exists()
            .then(|| KvmContext::from_dev_path("/dev/kvm").unwrap())
        else {
            return;
        };
        for kind in [BackingKind::AnonymousPrivate, BackingKind::SharedMemfd] {
            let (plan, vm, bindings, mut slots, population, snapshot) = restore_fixture(&ctx, kind);
            let mem = plan.memory_hotplug().unwrap();
            let window = mem.window_id();
            let geometry = mem.geometry();
            let first = GuestAddress(geometry.region().start().0);
            let second = GuestAddress(first.0 + geometry.chunk_size());
            vm.memory().view().write_obj(0xa1_u8, first).unwrap();
            vm.memory().view().write_obj(0xb2_u8, second).unwrap();
            let weak = Arc::downgrade(vm.memory().view());
            let permits = slots.allocate(2).unwrap();
            let overlap = slots
                .allocate(1)
                .unwrap()
                .pop()
                .unwrap()
                .bind_slice(window, geometry.chunk(1).unwrap().slice())
                .unwrap();
            let (mut resources, error) = match PreparedMem::restore(
                VIRTIO_MEM_ID,
                mem,
                Arc::clone(&vm),
                permits,
                population,
                snapshot,
            ) {
                Ok(_) => panic!("overlapping initial binding succeeded"),
                Err(failure) => failure,
            };
            assert!(matches!(
                error,
                AttachMemError::Service {
                    source: ServiceSetupError::Restore { chunk: 1, .. },
                    ..
                }
            ));
            assert_eq!(resources.bound_chunks(), 1);
            assert_eq!(resources.chunk_count(), 2);
            let (permit, mapping) = overlap.unbind().unwrap();
            drop((permit, mapping));
            resources.inject_unbind_failures(2);
            let mut pending = PendingCleanup::unstarted(
                vm,
                bindings,
                slots,
                BTreeMap::from([(window, resources)]),
            );
            for _ in 0..2 {
                let (owner, error) = pending.wait(Duration::ZERO).await.unwrap_err();
                assert!(
                    matches!(error, TeardownError::MemoryRelease { window: id, source: crate::memory::service::MemoryReleaseError { chunk: 0, .. } } if id == window)
                );
                assert!(
                    error
                        .source()
                        .unwrap()
                        .is::<crate::memory::service::MemoryReleaseError>()
                );
                let view = weak.upgrade().unwrap();
                assert_eq!(view.read_obj::<u8>(first).unwrap(), 0xa1);
                assert_eq!(view.read_obj::<u8>(second).unwrap(), 0xb2);
                pending = owner;
            }
            let memory = pending.wait(TIMEOUT).await.unwrap();
            assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), memory.view()));
            assert_eq!(memory.policy().kind, kind);
            assert_eq!(memory.view().read_obj::<u8>(first).unwrap(), 0xa1);
            assert_eq!(memory.view().read_obj::<u8>(second).unwrap(), 0xb2);
            drop(memory);
            assert!(weak.upgrade().is_none());
        }
    }

    #[tokio::test(flavor = "local")]
    async fn transport_validation_failure_after_restored_bind_returns_actual_resources() {
        let Some(ctx) = Path::new("/dev/kvm")
            .exists()
            .then(|| KvmContext::from_dev_path("/dev/kvm").unwrap())
        else {
            return;
        };
        for kind in [BackingKind::AnonymousPrivate, BackingKind::SharedMemfd] {
            let (plan, vm, bindings, mut slots, population, mut snapshot) =
                restore_fixture(&ctx, kind);
            let mem = plan.memory_hotplug().unwrap();
            snapshot.transport.desc.device_type = 25;
            let permits = slots.allocate(2).unwrap();
            let (resources, error) = match PreparedMem::restore(
                VIRTIO_MEM_ID,
                mem,
                Arc::clone(&vm),
                permits,
                population,
                snapshot,
            ) {
                Ok(_) => panic!("wrong descriptor was restored"),
                Err(failure) => failure,
            };
            assert!(matches!(
                error,
                AttachMemError::State {
                    source: RestoreMemError::Transport(RestoreError::Descriptor),
                    ..
                }
            ));
            assert_eq!(resources.bound_chunks(), 2);
            let pending = PendingCleanup::unstarted(
                vm,
                bindings,
                slots,
                BTreeMap::from([(mem.window_id(), resources)]),
            );
            pending.wait(TIMEOUT).await.unwrap();
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn platform_failure_retains_unattached_populated_memory_and_drops_unstarted_cpu_fds() {
        use crate::machine::build::{BuildError, PreparedDevice, assemble_parked};
        use crate::machine::platform::{Captured, ConsoleBackend, PlatformError, PlatformSnapshot};
        let Some(ctx) = Path::new("/dev/kvm")
            .exists()
            .then(|| KvmContext::from_dev_path("/dev/kvm").unwrap())
        else {
            return;
        };
        for kind in [BackingKind::AnonymousPrivate, BackingKind::SharedMemfd] {
            let (plan, vm, bindings, mut slots, population, snapshot) = restore_fixture(&ctx, kind);
            let mem = plan.memory_hotplug().unwrap();
            let permits = slots.allocate(2).unwrap();
            let prepared = PreparedMem::restore(
                VIRTIO_MEM_ID,
                mem,
                Arc::clone(&vm),
                permits,
                population,
                snapshot.clone(),
            )
            .unwrap();
            assert_eq!(prepared.transport.snapshot(), snapshot.transport);
            assert_eq!(
                prepared.interrupt.irq_fd().read().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            assert_eq!(
                prepared.queue_notifier.read().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            let weak = Arc::downgrade(vm.memory().view());
            let cpu = vm.create_vcpu(0).unwrap();
            let platform = PlatformSnapshot {
                i8042: Default::default(),
                serial: Some(Default::default()),
            };
            let (pending, error) = match assemble_parked(
                plan,
                vm,
                bindings,
                slots,
                vec![cpu],
                BTreeMap::from([(VIRTIO_MEM_ID.into(), PreparedDevice::Mem(prepared))]),
                &platform,
                ConsoleBackend::Captured(Captured::default()),
            ) {
                Ok(_) => panic!("platform started without a local runtime"),
                Err(failure) => failure,
            };
            assert!(matches!(
                error,
                BuildError::Platform(PlatformError::Spawn { .. })
            ));
            let memory = pending.wait(TIMEOUT).await.unwrap();
            assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), memory.view()));
            assert_eq!(memory.policy().kind, kind);
            drop(memory);
            assert!(weak.upgrade().is_none());
        }
    }

    #[test]
    fn failed_service_spawn_retains_populated_resources_across_runtime_join_timeout() {
        use std::sync::mpsc;
        use vm_memory::{Bytes, GuestAddress};
        let Some(ctx) = Path::new("/dev/kvm")
            .exists()
            .then(|| KvmContext::from_dev_path("/dev/kvm").unwrap())
        else {
            return;
        };
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        executor.block_on(async {
            for kind in [BackingKind::AnonymousPrivate, BackingKind::SharedMemfd] {
                let (plan, vm, bindings, mut memslots, population, snapshot) =
                    restore_fixture(&ctx, kind);
                let mem = plan.memory_hotplug().unwrap();
                let window = mem.window_id();
                let address = GuestAddress(mem.geometry().region().start().0);
                vm.memory().view().write_obj(0xa1_u8, address).unwrap();
                let weak = Arc::downgrade(vm.memory().view());
                let permits = memslots.allocate(2).unwrap();
                let prepared = PreparedMem::restore(
                    VIRTIO_MEM_ID,
                    mem,
                    Arc::clone(&vm),
                    permits,
                    population,
                    snapshot,
                )
                .unwrap();
                let (mut runtime, _exits) = DeviceRuntime::new();
                let task: DeviceHandle<()> = runtime
                    .spawn_on(
                        "completed",
                        Placement::Io,
                        Start::Running,
                        |control| async move {
                            drop(control);
                            Ok::<(), io::Error>(())
                        },
                    )
                    .unwrap();
                task.finished().await;
                let (entered_tx, entered_rx) = oneshot::channel();
                let (release_tx, release_rx) = mpsc::channel();
                let gate = tokio::task::spawn_blocking(move || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                });
                entered_rx.await.unwrap();
                assert!(matches!(
                    runtime.shutdown_wait(Duration::ZERO).await,
                    Err(ShutdownError::IoJoinTimeout(_))
                ));
                let mut parts = Box::new(CleanupParts {
                    vcpus: None,
                    runtime,
                    platform: None,
                    blocks: BTreeMap::new(),
                    mems: BTreeMap::new(),
                    memory_services: BTreeMap::new(),
                    memory_statuses: BTreeMap::new(),
                    unstarted_memory: BTreeMap::new(),
                    bindings,
                    memslots,
                    vm,
                });
                let error = prepared
                    .attach(VIRTIO_MEM_ID, mem, &mut parts, &mut Vec::new())
                    .unwrap_err();
                assert!(matches!(error, AttachMemError::Spawn { .. }));
                assert!(parts.memory_services.is_empty());
                assert_eq!(parts.unstarted_memory[&window].bound_chunks(), 2);
                let (pending, error) = PendingCleanup::tasks(parts)
                    .wait(Duration::ZERO)
                    .await
                    .unwrap_err();
                assert!(matches!(
                    error,
                    TeardownError::Devices(ShutdownError::IoJoinTimeout(_))
                ));
                assert_eq!(
                    weak.upgrade().unwrap().read_obj::<u8>(address).unwrap(),
                    0xa1
                );
                release_tx.send(()).unwrap();
                gate.await.unwrap();
                let memory = pending.wait(TIMEOUT).await.unwrap();
                assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), memory.view()));
                assert_eq!(memory.view().read_obj::<u8>(address).unwrap(), 0xa1);
                drop(memory);
                assert!(weak.upgrade().is_none());
            }
        });
    }

    #[test]
    fn setup_errors_keep_concrete_sources() {
        let error = AttachMemError::Service {
            id: "mem".into(),
            source: ServiceSetupError::PermitCount {
                expected: 2,
                actual: 1,
            },
        };
        assert!(matches!(
            error.source().unwrap().downcast_ref::<ServiceSetupError>(),
            Some(ServiceSetupError::PermitCount {
                expected: 2,
                actual: 1
            })
        ));
        let error = MachineResizeError::Resize(ResizeError::QueueFull);
        assert!(error.source().unwrap().is::<ResizeError>());
    }
}
