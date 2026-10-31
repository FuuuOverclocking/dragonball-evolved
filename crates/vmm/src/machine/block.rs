//! Ownership of a block transport and its two independently controlled tasks.

use std::sync::Arc;

use kvm_ioctls::IoEventAddress;

use super::plan::BlockPlan;
use crate::bus::BusDevice;
use crate::devices::runtime::{DeviceHandle, DeviceRuntime, Placement, SpawnError, Start};
use crate::devices::virtio::block::{BlockError, BlockHandle, BlockQuiesced, BlockResources};
use crate::devices::virtio::mmio::{MmioTransport, NOTIFY_OFFSET};
use crate::space::{Gpa, Range};
use crate::vm::KvmVm;

pub(super) type MmioDevices = Vec<(Range<Gpa>, Arc<dyn BusDevice<Gpa>>)>;

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum AttachBlockError {
    /// open disk {id}
    Open {
        id: String,
        #[source]
        source: BlockError,
    },
    /// register the queue notifier for disk {id}
    Ioevent {
        id: String,
        #[source]
        source: kvm_ioctls::Error,
    },
    /// register the interrupt for disk {id}
    Irqfd {
        id: String,
        #[source]
        source: kvm_ioctls::Error,
    },
    /// start tasks for disk {id}
    Spawn {
        id: String,
        #[source]
        source: SpawnError,
    },
}

/// Keyed by the plan's ID, not by probe position. Device state stays on the I/O thread.
pub(super) struct AttachedBlock {
    pub(super) worker: DeviceHandle<BlockQuiesced>,
    pub(super) resample: DeviceHandle<()>,
    pub(super) transport: Arc<MmioTransport>,
}

impl std::fmt::Debug for AttachedBlock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AttachedBlock")
            .field("worker", &self.worker)
            .field("resample", &self.resample)
            .finish_non_exhaustive()
    }
}

impl AttachedBlock {
    pub(super) fn attach(
        id: &str,
        plan: &BlockPlan,
        vm: &KvmVm,
        runtime: &mut DeviceRuntime,
        mmio: &mut MmioDevices,
    ) -> Result<Self, AttachBlockError> {
        let (resources, handle) = BlockResources::open(id, plan.path(), plan.read_only(), None)
            .map_err(|source| AttachBlockError::Open {
                id: id.into(),
                source,
            })?;
        let BlockHandle {
            activator,
            queue_notifier,
            interrupt,
            desc,
        } = handle;
        let grant = plan.grant();
        vm.fd()
            .register_ioevent(
                &queue_notifier,
                &IoEventAddress::Mmio(grant.mmio.start().0 + NOTIFY_OFFSET),
                0u32,
            )
            .map_err(|source| AttachBlockError::Ioevent {
                id: id.into(),
                source,
            })?;
        vm.fd()
            .register_irqfd_with_resample(interrupt.irq_fd(), interrupt.resample_fd(), grant.gsi.0)
            .map_err(|source| AttachBlockError::Irqfd {
                id: id.into(),
                source,
            })?;
        let irq = Arc::clone(&interrupt);
        let resample = runtime
            .spawn(format!("irq-{id}"), Start::Parked, move |control| {
                irq.resample(control)
            })
            .map_err(|source| AttachBlockError::Spawn {
                id: id.into(),
                source,
            })?;
        let worker = runtime
            .spawn_on(
                id,
                Placement::Io,
                Start::Parked,
                move |control| async move { resources.into_device()?.run(control).await },
            )
            .map_err(|source| AttachBlockError::Spawn {
                id: id.into(),
                source,
            })?;
        let transport = Arc::new(MmioTransport::new(
            desc,
            Arc::clone(vm.memory().view()),
            activator,
            interrupt,
            vec![queue_notifier],
        ));
        mmio.push((
            grant.mmio,
            Arc::clone(&transport) as Arc<dyn BusDevice<Gpa>>,
        ));
        Ok(Self {
            worker,
            resample,
            transport,
        })
    }
}
