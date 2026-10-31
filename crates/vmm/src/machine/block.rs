//! Ownership of a block transport and its two independently controlled tasks.

use std::io;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use kvm_ioctls::IoEventAddress;
use virtio_queue::{Queue, QueueState, QueueT};
use vmm_sys_util::eventfd::EventFd;

use super::plan::BlockPlan;
use super::state::QuiescedBlock;
use crate::bus::BusDevice;
use crate::devices::runtime::{
    DeviceHandle, DeviceRuntime, Placement, Response, SpawnError, Start,
};
use crate::devices::virtio::block::{BlockError, BlockHandle, BlockQuiesced, BlockResources};
use crate::devices::virtio::mmio::{
    DeviceDesc, MmioTransport, MmioTransportSnapshot, NOTIFY_OFFSET, RestoreError,
};
use crate::devices::virtio::{Activation, ActivationError, IrqTrigger, session_channel};
use crate::memory::store::MemoryView;
use crate::space::{Gpa, Range};
use crate::vm::KvmVm;

pub(super) type MmioDevices = Vec<(Range<Gpa>, Arc<dyn BusDevice<Gpa>>)>;

/// Pure device state; the enclosing map owns the disk ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSnapshot {
    pub transport: MmioTransportSnapshot,
    pub queue: Option<QueueState>,
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum RestoreBlockQueueError {
    /// saved block queue geometry or addresses do not match the transport
    Transport,
    /// saved block queue event-index mode does not match negotiated features
    Features,
    /// reconstruct the saved block queue
    Restore(#[source] virtio_queue::Error),
    /// saved block queue lies outside guest memory
    Bounds,
    /// read the saved block ring indices
    Ring(#[source] virtio_queue::Error),
    /// saved block ring positions are inconsistent
    Position,
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum AttachBlockError {
    /// open disk {id}
    Open {
        id: String,
        #[source]
        source: BlockError,
    },
    /// duplicate the queue notifier for disk {id}
    Notifier {
        id: String,
        #[source]
        source: io::Error,
    },
    /// restore the transport for disk {id}
    Restore {
        id: String,
        #[source]
        source: RestoreError,
    },
    /// restore the backend queue for disk {id}
    Queue {
        id: String,
        #[source]
        source: RestoreBlockQueueError,
    },
    /// queue the restored session for disk {id}
    Activate {
        id: String,
        #[source]
        source: ActivationError,
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
    /// reassert the pending block interrupt
    Interrupt(#[source] io::Error),
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum CaptureBlockError {
    /// block worker exited before capture
    WorkerExited,
    /// block worker did not quiesce in time
    WorkerTimedOut,
    /// block interrupt resampler exited before capture
    ResampleExited,
    /// block interrupt resampler did not quiesce in time
    ResampleTimedOut,
}

/// An opened, validated backend, with no KVM registrations or running tasks.
pub(super) struct PreparedBlock {
    resources: BlockResources,
    queue_notifier: EventFd,
    transport: Arc<MmioTransport>,
    interrupt: Arc<IrqTrigger>,
}

impl PreparedBlock {
    pub(super) fn cold(
        id: &str,
        plan: &BlockPlan,
        memory: &MemoryView,
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
        let notifier = queue_notifier
            .try_clone()
            .map_err(|source| AttachBlockError::Notifier {
                id: id.into(),
                source,
            })?;
        let mmio = MmioTransport::new(
            desc,
            Arc::clone(memory),
            activator,
            Arc::clone(&interrupt),
            vec![notifier],
        );
        Ok(Self {
            resources,
            queue_notifier,
            transport: Arc::new(mmio),
            interrupt,
        })
    }

    pub(super) fn restore(
        id: &str,
        plan: &BlockPlan,
        memory: &MemoryView,
        snapshot: BlockSnapshot,
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
        let queue = snapshot
            .queue
            .map(|state| restore_queue(state, &snapshot.transport, &desc, memory))
            .transpose()
            .map_err(|source| AttachBlockError::Queue {
                id: id.into(),
                source,
            })?;
        let notifier = queue_notifier
            .try_clone()
            .map_err(|source| AttachBlockError::Notifier {
                id: id.into(),
                source,
            })?;
        let driver_features = snapshot.transport.regs.driver_features;
        let restore_transport = |session| {
            MmioTransport::restore(
                desc,
                Arc::clone(memory),
                activator.clone(),
                Arc::clone(&interrupt),
                vec![notifier],
                snapshot.transport,
                session,
            )
            .map_err(|source| AttachBlockError::Restore {
                id: id.into(),
                source,
            })
        };
        // Backend state, not DRIVER_OK, owns activation and reset routing.
        let mmio = match queue {
            None => restore_transport(None)?,
            Some(queue) => {
                let (control, session) = session_channel();
                let mmio = restore_transport(Some(control))?;
                activator
                    .activate(Activation {
                        queues: vec![queue],
                        mem: Arc::clone(memory),
                        interrupt: Arc::clone(&interrupt),
                        driver_features,
                        session,
                    })
                    .map_err(|source| AttachBlockError::Activate {
                        id: id.into(),
                        source,
                    })?;
                mmio
            }
        };
        Ok(Self {
            resources,
            queue_notifier,
            transport: Arc::new(mmio),
            interrupt,
        })
    }

    pub(super) fn attach(
        self,
        id: &str,
        plan: &BlockPlan,
        vm: &KvmVm,
        runtime: &mut DeviceRuntime,
        mmio: &mut MmioDevices,
    ) -> Result<AttachedBlock, AttachBlockError> {
        let grant = plan.grant();
        vm.fd()
            .register_ioevent(
                &self.queue_notifier,
                &IoEventAddress::Mmio(grant.mmio.start().0 + NOTIFY_OFFSET),
                0u32,
            )
            .map_err(|source| AttachBlockError::Ioevent {
                id: id.into(),
                source,
            })?;
        let interrupt = &self.interrupt;
        vm.fd()
            .register_irqfd_with_resample(interrupt.irq_fd(), interrupt.resample_fd(), grant.gsi.0)
            .map_err(|source| AttachBlockError::Irqfd {
                id: id.into(),
                source,
            })?;
        let attached = self.spawn(id, runtime)?;
        mmio.push((
            grant.mmio,
            Arc::clone(&attached.transport) as Arc<dyn BusDevice<Gpa>>,
        ));
        Ok(attached)
    }

    fn spawn(
        self,
        id: &str,
        runtime: &mut DeviceRuntime,
    ) -> Result<AttachedBlock, AttachBlockError> {
        let Self {
            resources,
            queue_notifier: _,
            transport,
            interrupt,
        } = self;
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
        Ok(AttachedBlock {
            worker,
            resample,
            transport,
            interrupt,
        })
    }
}

fn restore_queue(
    state: QueueState,
    transport: &MmioTransportSnapshot,
    desc: &DeviceDesc,
    memory: &MemoryView,
) -> Result<Queue, RestoreBlockQueueError> {
    let [regs] = transport.regs.queues.as_slice() else {
        return Err(RestoreBlockQueueError::Transport);
    };
    if desc.queue_max.as_slice() != [state.max_size]
        || u32::from(state.size) != regs.size
        || state.ready != regs.ready
        || !state.ready
        || state.desc_table != regs.desc.0
        || state.avail_ring != regs.avail.0
        || state.used_ring != regs.used.0
    {
        return Err(RestoreBlockQueueError::Transport);
    }
    const EVENT_IDX: u64 = 1 << 29;
    if state.event_idx_enabled != (transport.regs.driver_features & EVENT_IDX != 0) {
        return Err(RestoreBlockQueueError::Features);
    }
    let queue = Queue::try_from(state).map_err(RestoreBlockQueueError::Restore)?;
    if !queue.is_valid(memory.as_ref()) {
        return Err(RestoreBlockQueueError::Bounds);
    }
    // Quiesce completes synchronous chains, so no in-flight queue position can be restored.
    let available = queue
        .avail_idx(memory.as_ref(), Ordering::Acquire)
        .map_err(RestoreBlockQueueError::Ring)?
        .0;
    let used = queue
        .used_idx(memory.as_ref(), Ordering::Acquire)
        .map_err(RestoreBlockQueueError::Ring)?
        .0;
    if state.next_avail != state.next_used
        || used != state.next_used
        || available.wrapping_sub(state.next_avail) > state.size
    {
        return Err(RestoreBlockQueueError::Position);
    }
    Ok(queue)
}

/// Keyed by the plan's ID, not by probe position. Device state stays on the I/O thread.
pub(super) struct AttachedBlock {
    pub(super) worker: DeviceHandle<BlockQuiesced>,
    pub(super) resample: DeviceHandle<()>,
    pub(super) transport: Arc<MmioTransport>,
    interrupt: Arc<IrqTrigger>,
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
    /// CPUs must remain paused after the settled quiesce reply to exclude new resets.
    pub(super) fn capture(
        &self,
        quiesced: &QuiescedBlock,
    ) -> Result<BlockSnapshot, CaptureBlockError> {
        let queue = match quiesced.worker {
            Response::Replied(BlockQuiesced::Inactive) => None,
            Response::Replied(BlockQuiesced::Active(queue)) => Some(queue),
            Response::Exited => return Err(CaptureBlockError::WorkerExited),
            Response::TimedOut => return Err(CaptureBlockError::WorkerTimedOut),
        };
        match quiesced.resample {
            Response::Replied(()) => {}
            Response::Exited => return Err(CaptureBlockError::ResampleExited),
            Response::TimedOut => return Err(CaptureBlockError::ResampleTimedOut),
        }
        self.check_alive()?;
        let transport = self.transport.snapshot();
        self.check_alive()?;
        Ok(BlockSnapshot { transport, queue })
    }

    fn check_alive(&self) -> Result<(), CaptureBlockError> {
        if self.worker.has_exited() {
            return Err(CaptureBlockError::WorkerExited);
        }
        if self.resample.has_exited() {
            return Err(CaptureBlockError::ResampleExited);
        }
        Ok(())
    }

    /// Delay IRQ injection until CPU/LAPIC/clock restoration.
    pub(super) fn reraise_pending(&self) -> Result<(), AttachBlockError> {
        self.interrupt
            .reraise_pending()
            .map_err(AttachBlockError::Interrupt)
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use api::RawVmmOp;
    use api::block::{AddDisk, BlockDevConfig};
    use api::boot::{KernelConfig, SetKernel};
    use tokio::time;
    use virtio_queue::desc::split::Descriptor;
    use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

    use super::*;
    use crate::devices::virtio::mmio::{QueueError, QueueRegs, VERSION_1};
    use crate::error::render_chain;
    use crate::kvm_context::HostCapabilities;
    use crate::machine::plan::MachinePlan;
    use crate::spec::MachineSpec;
    use crate::test_guest::TempFile;

    const TIMEOUT: Duration = Duration::from_secs(5);
    const ID: &str = "disk";

    fn plan(path: &Path) -> BlockPlan {
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
                AddDisk(BlockDevConfig {
                    id: Some(ID.into()),
                    path: path.into(),
                    read_only: true,
                    root: false,
                })
                .into_vmm_command(),
            )
            .unwrap();
        MachinePlan::new(&spec, &HostCapabilities::fake(46, 32, 4096))
            .unwrap()
            .0
            .devices()[ID]
            .block()
            .clone()
    }

    fn memory() -> MemoryView {
        Arc::new(GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap())
    }

    fn snapshot(plan: &BlockPlan, memory: &MemoryView) -> BlockSnapshot {
        let prepared = PreparedBlock::cold(ID, plan, memory).unwrap();
        let mut transport = prepared.transport.snapshot();
        transport.regs.status = 15;
        transport.regs.driver_features = VERSION_1;
        transport.regs.queues[0] = QueueRegs {
            size: 16,
            ready: true,
            desc: Gpa(0x1000),
            avail: Gpa(0x2000),
            used: Gpa(0x3000),
        };
        let queue = transport.regs.queues[0]
            .queue(transport.desc.queue_max[0], memory)
            .unwrap()
            .state();
        BlockSnapshot {
            transport,
            queue: Some(queue),
        }
    }

    fn pending_read(memory: &MemoryView) {
        memory.write_obj([0u8; 16], GuestAddress(0x4000)).unwrap();
        for (address, descriptor) in [
            (0x1000, Descriptor::new(0x4000, 16, 1, 1)),
            (0x1010, Descriptor::new(0x5000, 512, 3, 2)),
            (0x1020, Descriptor::new(0x8000, 1, 2, 0)),
        ] {
            memory.write_obj(descriptor, GuestAddress(address)).unwrap();
        }
        memory.write_obj(0xffu8, GuestAddress(0x8000)).unwrap();
        memory.write_obj(0u16, GuestAddress(0x2004)).unwrap();
        memory
            .write_obj(1u16.to_le(), GuestAddress(0x2002))
            .unwrap();
    }

    async fn quiesce(block: &AttachedBlock) -> QuiescedBlock {
        QuiescedBlock {
            worker: block.worker.quiesce(TIMEOUT).await,
            resample: block.resample.quiesce(TIMEOUT).await,
        }
    }

    #[test]
    fn backend_queue_must_match_every_transport_property_and_feature_mode() {
        let disk = TempFile::new(&[0xab; 512]);
        let plan = plan(disk.path());
        let memory = memory();
        let saved = snapshot(&plan, &memory);
        let mutations: [fn(&mut QueueState); 7] = [
            |q| q.max_size = 128,
            |q| q.size = 8,
            |q| q.ready = false,
            |q| q.desc_table += 16,
            |q| q.avail_ring += 2,
            |q| q.used_ring += 4,
            |q| q.event_idx_enabled = true,
        ];
        for mutate in mutations {
            let mut bad = saved.clone();
            mutate(bad.queue.as_mut().unwrap());
            assert!(matches!(
                PreparedBlock::restore(ID, &plan, &memory, bad),
                Err(AttachBlockError::Queue { .. })
            ));
        }
        let mut bad = saved;
        bad.queue.as_mut().unwrap().ready = false;
        bad.transport.regs.queues[0].ready = false;
        assert!(matches!(
            PreparedBlock::restore(ID, &plan, &memory, bad),
            Err(AttachBlockError::Queue {
                source: RestoreBlockQueueError::Transport,
                ..
            })
        ));
    }

    #[test]
    fn preparation_rejects_changed_descriptors_and_invalid_transport_queues() {
        let disk = TempFile::new(&[0xab; 512]);
        let plan = plan(disk.path());
        let memory = memory();
        let saved = snapshot(&plan, &memory);
        for field in 0..4 {
            let mut bad = saved.clone();
            match field {
                0 => bad.transport.desc.device_type = 1,
                1 => bad.transport.desc.features ^= 1 << 5,
                2 => bad.transport.desc.queue_max[0] = 128,
                _ => bad.transport.desc.config = 2u64.to_le_bytes().to_vec(),
            }
            assert!(matches!(
                PreparedBlock::restore(ID, &plan, &memory, bad),
                Err(AttachBlockError::Restore {
                    source: RestoreError::Descriptor,
                    ..
                })
            ));
        }
        let mutations: [fn(&mut QueueRegs); 5] = [
            |q| q.size = 0,
            |q| q.size = 3,
            |q| q.size = 512,
            |q| q.desc = Gpa(0x1001),
            |q| q.used = Gpa(0xfffc),
        ];
        for mutate in mutations {
            let mut bad = saved.clone();
            bad.queue = None;
            mutate(&mut bad.transport.regs.queues[0]);
            assert!(matches!(
                PreparedBlock::restore(ID, &plan, &memory, bad),
                Err(AttachBlockError::Restore {
                    source: RestoreError::Queue { .. },
                    ..
                })
            ));
        }
        fs::write(disk.path(), [0xab; 1024]).unwrap();
        assert!(matches!(
            PreparedBlock::restore(ID, &plan, &memory, saved),
            Err(AttachBlockError::Restore {
                source: RestoreError::Descriptor,
                ..
            })
        ));
        fs::write(disk.path(), [0u8; 511]).unwrap();
        assert!(matches!(
            PreparedBlock::cold(ID, &plan, &memory),
            Err(AttachBlockError::Open {
                source: BlockError::TooSmall,
                ..
            })
        ));
    }

    #[test]
    fn backend_alignment_bounds_and_ring_positions_are_validated_before_spawn() {
        let disk = TempFile::new(&[0xab; 512]);
        let plan = plan(disk.path());
        let memory = memory();
        let saved = snapshot(&plan, &memory);
        let mut bad = saved.clone();
        bad.queue.as_mut().unwrap().desc_table = 0x1001;
        bad.transport.regs.queues[0].desc = Gpa(0x1001);
        assert!(matches!(
            PreparedBlock::restore(ID, &plan, &memory, bad),
            Err(AttachBlockError::Queue {
                source: RestoreBlockQueueError::Restore(_),
                ..
            })
        ));
        let mut bad = saved.clone();
        bad.queue.as_mut().unwrap().used_ring = 0xfffc;
        bad.transport.regs.queues[0].used = Gpa(0xfffc);
        assert!(matches!(
            PreparedBlock::restore(ID, &plan, &memory, bad),
            Err(AttachBlockError::Queue {
                source: RestoreBlockQueueError::Bounds,
                ..
            })
        ));
        let mut bad = saved.clone();
        bad.queue.as_mut().unwrap().next_avail = 1;
        assert!(matches!(
            PreparedBlock::restore(ID, &plan, &memory, bad),
            Err(AttachBlockError::Queue {
                source: RestoreBlockQueueError::Position,
                ..
            })
        ));
        for (available, used) in [(17u16, 0u16), (0, 1)] {
            memory
                .write_obj(available.to_le(), GuestAddress(0x2002))
                .unwrap();
            memory
                .write_obj(used.to_le(), GuestAddress(0x3002))
                .unwrap();
            assert!(matches!(
                PreparedBlock::restore(ID, &plan, &memory, saved.clone()),
                Err(AttachBlockError::Queue {
                    source: RestoreBlockQueueError::Position,
                    ..
                })
            ));
        }
        // Ring positions are wrapping counters, not indices bounded by size.
        let mut wrapped = saved;
        let queue = wrapped.queue.as_mut().unwrap();
        queue.next_avail = u16::MAX;
        queue.next_used = u16::MAX;
        memory
            .write_obj(1u16.to_le(), GuestAddress(0x2002))
            .unwrap();
        memory
            .write_obj(u16::MAX.to_le(), GuestAddress(0x3002))
            .unwrap();
        assert!(PreparedBlock::restore(ID, &plan, &memory, wrapped).is_ok());
    }

    #[tokio::test(flavor = "local")]
    async fn restored_activation_parks_without_io_and_irq_reassertion_is_explicit() {
        let disk = TempFile::new(&[0xab; 512]);
        let plan = plan(disk.path());
        for status in [11, 15] {
            let memory = memory();
            pending_read(&memory);
            let mut saved = snapshot(&plan, &memory);
            saved.transport.regs.status = status;
            saved.interrupt_status = 3;
            let mut before = vec![0; 0x10000];
            memory.read_slice(&mut before, GuestAddress(0)).unwrap();
            let prepared = PreparedBlock::restore(ID, &plan, &memory, saved.clone()).unwrap();
            assert_eq!(
                prepared.interrupt.irq_fd().read().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            let (mut runtime, _exits) = DeviceRuntime::new();
            let block = prepared.spawn(ID, &mut runtime).unwrap();
            let quiesced = quiesce(&block).await;
            assert_eq!(block.capture(&quiesced).unwrap(), saved);
            let mut after = vec![0; before.len()];
            memory.read_slice(&mut after, GuestAddress(0)).unwrap();
            assert_eq!(after, before);
            assert_eq!(
                block.interrupt.irq_fd().read().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            block.reraise_pending().unwrap();
            assert_eq!(block.interrupt.irq_fd().read().unwrap(), 1);
            assert_eq!(block.capture(&quiesced).unwrap(), saved);
            block.transport.write(0x64, &3u32.to_le_bytes());
            block.reraise_pending().unwrap();
            assert_eq!(
                block.interrupt.irq_fd().read().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            // Reset must reach the restored backend session even while parked.
            block.transport.write(0x70, &0u32.to_le_bytes());
            let reset = block.capture(&quiesce(&block).await).unwrap();
            assert_eq!(reset.transport.regs.status, 0);
            assert_eq!(reset.interrupt_status, 0);
            assert_eq!(reset.queue, None);
            memory.read_slice(&mut after, GuestAddress(0)).unwrap();
            assert_eq!(after, before);
            runtime.shutdown(TIMEOUT).await.unwrap();
        }
    }

    #[tokio::test(flavor = "local")]
    async fn inactive_driver_ok_does_not_reconstruct_or_activate_a_backend_queue() {
        let disk = TempFile::new(&[0xab; 512]);
        let plan = plan(disk.path());
        let memory = memory();
        pending_read(&memory);
        let mut saved = snapshot(&plan, &memory);
        saved.queue = None;
        let prepared = PreparedBlock::restore(ID, &plan, &memory, saved.clone()).unwrap();
        let (mut runtime, _exits) = DeviceRuntime::new();
        let block = prepared.spawn(ID, &mut runtime).unwrap();
        assert_eq!(block.capture(&quiesce(&block).await).unwrap(), saved);
        assert_eq!(block.worker.resume(TIMEOUT).await, Response::Replied(()));
        block.transport.write(NOTIFY_OFFSET, &0u32.to_le_bytes());
        assert_eq!(block.capture(&quiesce(&block).await).unwrap(), saved);
        assert_eq!(memory.read_obj::<u16>(GuestAddress(0x3002)).unwrap(), 0);
        assert_eq!(memory.read_obj::<u8>(GuestAddress(0x8000)).unwrap(), 0xff);
        runtime.shutdown(TIMEOUT).await.unwrap();
    }

    #[tokio::test(flavor = "local")]
    async fn attachment_uses_the_validated_open_file_even_if_the_path_is_replaced() {
        let disk = TempFile::new(&[0xab; 512]);
        let replacement = TempFile::new(&[0xcd; 1024]);
        let plan = plan(disk.path());
        let memory = memory();
        pending_read(&memory);
        let saved = snapshot(&plan, &memory);
        let prepared = PreparedBlock::restore(ID, &plan, &memory, saved.clone()).unwrap();
        fs::rename(replacement.path(), disk.path()).unwrap();
        let (mut runtime, _exits) = DeviceRuntime::new();
        let block = prepared.spawn(ID, &mut runtime).unwrap();
        assert_eq!(block.capture(&quiesce(&block).await).unwrap(), saved);
        assert_eq!(block.worker.resume(TIMEOUT).await, Response::Replied(()));
        time::timeout(TIMEOUT, async {
            while memory.read_obj::<u16>(GuestAddress(0x3002)).unwrap() == 0 {
                assert!(!block.worker.has_exited());
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let quiesced = quiesce(&block).await;
        let captured = block.capture(&quiesced).unwrap();
        assert_eq!(captured.queue.unwrap().next_used, 1);
        assert_eq!(memory.read_obj::<u8>(GuestAddress(0x5000)).unwrap(), 0xab);
        assert_eq!(memory.read_obj::<u8>(GuestAddress(0x8000)).unwrap(), 0);
        runtime.shutdown(TIMEOUT).await.unwrap();
    }

    #[tokio::test(flavor = "local")]
    async fn capture_requires_both_replies_and_rejects_late_exits_in_either_task() {
        let disk = TempFile::new(&[0xab; 512]);
        let plan = plan(disk.path());
        for stop_worker in [true, false] {
            let (mut runtime, _exits) = DeviceRuntime::new();
            let block = PreparedBlock::cold(ID, &plan, &memory())
                .unwrap()
                .spawn(ID, &mut runtime)
                .unwrap();
            let replied = quiesce(&block).await;
            assert!(block.capture(&replied).is_ok());
            for response in [Response::Exited, Response::TimedOut] {
                let worker = QuiescedBlock {
                    worker: response,
                    ..replied
                };
                assert!(matches!(
                    block.capture(&worker),
                    Err(CaptureBlockError::WorkerExited | CaptureBlockError::WorkerTimedOut)
                ));
            }
            for response in [Response::Exited, Response::TimedOut] {
                let resample = QuiescedBlock {
                    resample: response,
                    ..replied
                };
                assert!(matches!(
                    block.capture(&resample),
                    Err(CaptureBlockError::ResampleExited | CaptureBlockError::ResampleTimedOut)
                ));
            }
            if stop_worker {
                block.worker.stop();
                time::timeout(TIMEOUT, block.worker.finished())
                    .await
                    .unwrap();
                assert!(matches!(
                    block.capture(&replied),
                    Err(CaptureBlockError::WorkerExited)
                ));
            } else {
                block.resample.stop();
                time::timeout(TIMEOUT, block.resample.finished())
                    .await
                    .unwrap();
                assert!(matches!(
                    block.capture(&replied),
                    Err(CaptureBlockError::ResampleExited)
                ));
            }
            runtime.shutdown(TIMEOUT).await.unwrap();
        }
    }

    #[test]
    fn dropping_prepared_activation_releases_all_ram_views() {
        let disk = TempFile::new(&[0xab; 512]);
        let plan = plan(disk.path());
        let memory = memory();
        let weak = Arc::downgrade(&memory);
        let saved = snapshot(&plan, &memory);
        let prepared = PreparedBlock::restore(ID, &plan, &memory, saved).unwrap();
        drop(memory);
        drop(prepared);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn errors_keep_context_separate_from_typed_sources() {
        let error = AttachBlockError::Restore {
            id: ID.into(),
            source: RestoreError::Queue {
                index: 0,
                error: QueueError::Alignment,
            },
        };
        let source = error
            .source()
            .unwrap()
            .downcast_ref::<RestoreError>()
            .unwrap();
        assert!(matches!(
            source.source().unwrap().downcast_ref::<QueueError>(),
            Some(QueueError::Alignment)
        ));
        assert_eq!(
            render_chain(&error),
            "restore the transport for disk disk: invalid saved virtio queue 0: virtio queue addresses are misaligned"
        );
        let error = AttachBlockError::Queue {
            id: ID.into(),
            source: RestoreBlockQueueError::Restore(Queue::new(0).unwrap_err()),
        };
        let source = error
            .source()
            .unwrap()
            .downcast_ref::<RestoreBlockQueueError>()
            .unwrap();
        assert!(source.source().unwrap().is::<virtio_queue::Error>());
        let error = AttachBlockError::Interrupt(io::Error::other("IRQ write failed"));
        assert!(error.source().unwrap().is::<io::Error>());
        assert_eq!(
            render_chain(&error),
            "reassert the pending block interrupt: IRQ write failed"
        );
    }
}
