//! virtio-blk disks behind virtio-mmio transports: planning, attaching,
//! quiescing and capturing them.

use std::path::PathBuf;
use std::range::Range;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use kvm_ioctls::{IoEventAddress, VmFd};
use linux_loader::cmdline::Cmdline;
use logger::warn_unlimited;
use virtio_queue::{Queue, QueueState, QueueT};
use vm_memory::GuestAddress;

use super::bus::BusDevice;
use super::memory::SharedGuestMemory;
use super::plan::VirtioMmioGrant;
use crate::devices::virtio::block::{
    BlockHandle, BlockQuiesceHandle, BlockQuiesced, BlockResources,
};
use crate::devices::virtio::mmio::{MmioTransport, MmioTransportSnapshot, NOTIFY_OFFSET};
use crate::devices::virtio::{Activation, session_channel};
use crate::devices::{DeviceManager, DeviceRole, Placement};
use crate::resources::ResourceBook;
use crate::spec::BlockSpec;

/// A disk's place in the machine: its backing file and the transport grant
/// the guest reaches it through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockPlan {
    pub id: String,
    pub path: PathBuf,
    pub read_only: bool,
    pub root: bool,
    pub grant: VirtioMmioGrant,
}

/// A disk's captured state: its transport registers and, if its backend had a
/// session, the queue it was serving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSnapshot {
    pub id: String,
    pub transport: MmioTransportSnapshot,
    pub queue: Option<QueueState>,
}

/// A disk attached to a machine.
pub(super) struct AttachedBlock {
    plan: BlockPlan,
    transport: Arc<MmioTransport>,
    quiesce: BlockQuiesceHandle,
    /// What the backend was doing when the machine last paused; `None` while
    /// the backend runs, since its queue then moves on.
    state: Option<BlockQuiesced>,
}

/// Lays out the disks: at most one root disk, placed first so the guest names
/// it `/dev/vda`, each granted a transport window and line from `book`.
pub fn plan<'a>(
    specs: impl IntoIterator<Item = &'a BlockSpec>,
    mut book: ResourceBook,
    cmdline: &mut Cmdline,
) -> Result<(Vec<BlockPlan>, ResourceBook)> {
    let mut specs: Vec<_> = specs.into_iter().collect();
    if specs.iter().filter(|spec| spec.root).count() > 1 {
        bail!("multiple root disks");
    }
    specs.sort_by_key(|spec| !spec.root);
    let mut plans = Vec::with_capacity(specs.len());
    for spec in specs {
        let (grant, next) = VirtioMmioGrant::allocate(&book)
            .with_context(|| format!("allocate virtio-mmio resources for disk {}", spec.id))?;
        book = next;
        cmdline.add_virtio_mmio_device(
            grant.mmio.len(),
            GuestAddress(grant.mmio.start()),
            grant.gsi.0,
            None,
        )?;
        if spec.root {
            cmdline.insert("root", "/dev/vda")?;
            cmdline.insert_str(if spec.read_only { "ro" } else { "rw" })?;
        }
        plans.push(BlockPlan {
            id: spec.id.clone(),
            path: spec.path.clone(),
            read_only: spec.read_only,
            root: spec.root,
            grant,
        });
    }
    Ok((plans, book))
}

/// Opens the disk and wires it in at its grant: the queue notifier as an
/// ioeventfd, the interrupt as a resampled irqfd, the transport on the MMIO bus
/// and the backend on the I/O thread.
///
/// Restoring brings the transport back as captured, and a backend that had a
/// session is reactivated with its captured queue.
pub(super) fn attach(
    plan: BlockPlan,
    vm: &VmFd,
    memory: &SharedGuestMemory,
    tasks: &mut DeviceManager,
    mmio_bus: &mut Vec<(Range<u64>, Arc<dyn BusDevice>)>,
    restore: Option<BlockSnapshot>,
) -> Result<AttachedBlock> {
    let (resources, handle, quiesce) =
        BlockResources::open(&plan.id, &plan.path, plan.read_only, None)?;
    let BlockHandle {
        activator,
        queue_notifier,
        interrupt,
        desc,
    } = handle;
    vm.register_ioevent(
        &queue_notifier,
        &IoEventAddress::Mmio(plan.grant.mmio.start() + NOTIFY_OFFSET),
        0u32,
    )?;
    vm.register_irqfd_with_resample(
        interrupt.irq_fd(),
        interrupt.resample_fd(),
        plan.grant.gsi.0,
    )?;
    let stop = tasks.stop_signal();
    tasks.spawn(
        format!("irq-{}", plan.grant.gsi.0),
        DeviceRole::GuestAffecting,
        Arc::clone(&interrupt).resample(stop),
    );

    // The backend's record, not the transport status, decides reactivation.
    let (transport, reactivate) = match restore {
        None => (
            MmioTransport::new(
                desc,
                Arc::clone(memory),
                activator,
                interrupt,
                vec![queue_notifier],
            ),
            None,
        ),
        Some(captured) => {
            let driver_features = captured.transport.regs.driver_features;
            let pending = captured.transport.interrupt_status != 0;
            let transport = MmioTransport::restore(
                desc,
                Arc::clone(memory),
                activator.clone(),
                Arc::clone(&interrupt),
                vec![queue_notifier],
                captured.transport,
            )
            .with_context(|| format!("restore disk {}", plan.id))?;
            // Validated here, so a bad capture fails the restore instead of the backend.
            let queue = captured
                .queue
                .map(|queue| {
                    let queue = Queue::try_from(queue).context("restore block queue from state")?;
                    if !queue.is_valid(memory.as_ref()) {
                        bail!("restored block queue failed validation");
                    }
                    Ok(queue)
                })
                .transpose()
                .with_context(|| format!("restore disk {}", plan.id))?;
            let reactivate = queue.map(|queue| {
                // Injected before the backend runs, so a guest reset reaches the restored session.
                let (control, session) = session_channel();
                transport.inject_session(control);
                let mem = Arc::clone(memory);
                let interrupt = Arc::clone(&interrupt);
                move || {
                    activator.activate(Activation {
                        mem,
                        queues: vec![queue],
                        interrupt,
                        driver_features,
                        session,
                    })
                }
            });
            // KVM injects an irqfd write asynchronously, so an interrupt raised
            // just before the capture may be missing from the captured irqchip.
            if pending {
                interrupt
                    .irq_fd()
                    .write(1)
                    .context("re-raise the restored interrupt")?;
            }
            (transport, reactivate)
        }
    };
    let transport = Arc::new(transport);
    mmio_bus.push((
        plan.grant.mmio.into(),
        Arc::clone(&transport) as Arc<dyn BusDevice>,
    ));
    tasks.spawn_on(
        plan.id.clone(),
        Placement::Io,
        DeviceRole::HostOnly,
        move |stop| async move {
            let device = resources.into_device()?;
            if let Some(reactivate) = reactivate {
                reactivate()?;
            }
            device.run(stop).await
        },
    )?;
    Ok(AttachedBlock {
        plan,
        transport,
        quiesce,
        state: None,
    })
}

impl AttachedBlock {
    /// Parks the backend and records what it was doing. An exited backend is
    /// recorded, not an error, so pausing survives a failed disk.
    pub(super) async fn quiesce(&mut self) {
        let state = self.quiesce.quiesce().await;
        if state == BlockQuiesced::Exited {
            warn_unlimited!(
                "block device {} has exited; pausing without it",
                self.plan.id
            );
        }
        self.state = Some(state);
    }

    pub(super) async fn resume(&mut self) {
        self.state = None;
        self.quiesce.resume().await;
    }

    /// Captures the disk as of the last pause. A running backend's queue moves
    /// on, and an exited backend has no state: restoring it would hide the failure.
    pub(super) fn capture(&self) -> Result<BlockSnapshot> {
        let queue = match self.state {
            None => bail!(
                "block device {} is running; pause the machine to capture it",
                self.plan.id
            ),
            Some(BlockQuiesced::Inactive) => None,
            Some(BlockQuiesced::Active(queue)) => Some(queue),
            Some(BlockQuiesced::Exited) => bail!(
                "block device {} has exited; refusing to snapshot",
                self.plan.id
            ),
        };
        Ok(BlockSnapshot {
            id: self.plan.id.clone(),
            transport: self.transport.snapshot(),
            queue,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::x86_64::layout::{CMDLINE_MAX_SIZE, resource_book};

    fn disk(id: &str, root: bool) -> BlockSpec {
        BlockSpec {
            id: id.into(),
            path: format!("/{id}").into(),
            read_only: false,
            root,
        }
    }

    #[test]
    fn only_one_root_disk_is_planned_and_it_comes_first() {
        let mut cmdline = Cmdline::new(CMDLINE_MAX_SIZE).unwrap();
        let roots = [disk("a", true), disk("b", true)];
        let Err(error) = plan(&roots, resource_book(), &mut cmdline) else {
            panic!("planned two root disks");
        };
        assert_eq!(error.to_string(), "multiple root disks");

        let (plans, _) = plan(
            &[disk("data", false), disk("root", true)],
            resource_book(),
            &mut cmdline,
        )
        .unwrap();
        assert_eq!(
            plans
                .iter()
                .map(|plan| plan.id.as_str())
                .collect::<Vec<_>>(),
            ["root", "data"]
        );
    }
}
