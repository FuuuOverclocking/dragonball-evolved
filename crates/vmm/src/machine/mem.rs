use std::io;
use std::sync::Arc;

use kvm_ioctls::IoEventAddress;
use vmm_sys_util::eventfd::EventFd;

use super::block::MmioDevices;
use super::cleanup::CleanupParts;
use super::plan::MemPlan;
use crate::bus::BusDevice;
use crate::devices::runtime::{DeviceHandle, Placement, SpawnError, Start};
use crate::devices::virtio::IrqTrigger;
use crate::devices::virtio::mem::{
    MemError, MemHandle, MemQuiesced, MemResources, ResizeError, ResizeHandle,
};
use crate::devices::virtio::mmio::{
    MmioTransport, MmioTransportSnapshot, NOTIFY_OFFSET, TransportError,
};
use crate::memory::hotplug::Population;
use crate::memory::service::{MemoryService, ServiceSetupError, WindowStatus};
use crate::resources::IdPoolError;
use crate::space::{Gpa, MemSlot};
use crate::vm::{KvmVm, SlotPermit};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemSnapshot {
    pub transport: MmioTransportSnapshot,
    pub state: MemQuiesced,
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
                    drop(service);
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
