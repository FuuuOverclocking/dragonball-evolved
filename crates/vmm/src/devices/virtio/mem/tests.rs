use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Waker};
use std::time::Duration;

use virtio_queue::desc::split::Descriptor;
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

use super::protocol::{BlockState, Config, Request, Response};
use super::*;
use crate::align::BackingPageSize;
use crate::bus::BusDevice;
use crate::devices::runtime::{
    DeviceExits, DeviceFailure, DeviceHandle, DeviceRuntime, Placement,
    Response as ControlResponse, Start,
};
use crate::devices::virtio::mmio::MmioTransport;
use crate::devices::virtio::{Activation, ResetError, SessionControl, session_channel};
use crate::kvm_context::KvmContext;
use crate::memory::hotplug::Population;
use crate::memory::layout::{
    MemoryLayout, Object, ObjectId, Window, WindowId, WindowKind, WindowOwner,
};
use crate::memory::policy::{BackingKind, HostPolicy};
use crate::memory::service::{MemoryHandle, MemoryService};
use crate::memory::store::{MemoryStore, MemoryView};
use crate::space::{Gpa, ObjectOffset};
use crate::vm::KvmVm;

const BLOCK: u64 = 0x1000;
const CHUNK: u64 = 4 * BLOCK;
const SIZE: u64 = 3 * CHUNK;
const START: Gpa = Gpa(1 << 30);
const WITHIN: Duration = Duration::from_secs(5);
const DESC: u64 = 0x1000;
const AVAIL: u64 = 0x2000;
const USED: u64 = 0x3000;
const PAYLOAD: u64 = 0x5000;

fn assert_pending<F: Future>(request: Pin<&mut F>) {
    assert!(
        request
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
}

fn enqueue<F: Future>(request: F) -> Pin<Box<F>> {
    let mut request = Box::pin(request);
    assert_pending(request.as_mut());
    request
}

fn restore_seed(next_avail: u16, next_used: u16) -> MemRestore {
    let config = Config {
        block_size: BLOCK,
        node_id: 0,
        addr: START,
        region_size: SIZE,
        usable_region_size: CHUNK,
        plugged_size: 0,
        requested_size: BLOCK,
    };
    MemRestore {
        state: MemQuiesced {
            requested_size: BLOCK,
            usable_region_size: CHUNK,
            memory_revision: 0,
            queue: Some(virtio_queue::QueueState {
                max_size: protocol::QUEUE_MAX,
                next_avail,
                next_used,
                event_idx_enabled: false,
                size: protocol::QUEUE_MAX,
                ready: true,
                desc_table: DESC,
                avail_ring: AVAIL,
                used_ring: USED,
            }),
        },
        config: ConfigSnapshot {
            bytes: config.encode().to_vec(),
            generation: 7,
        },
    }
}

fn kvm() -> Option<KvmContext> {
    Path::new("/dev/kvm")
        .exists()
        .then(|| KvmContext::from_dev_path("/dev/kvm").unwrap())
}

fn range(first: u64, count: u64) -> Range<Gpa> {
    Range::new(Gpa(START.0 + first * BLOCK), count * BLOCK).unwrap()
}

async fn wait(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(WITHIN, async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

struct Guest {
    mem: MemoryView,
    submitted: u16,
}

impl Guest {
    fn queue(&self) -> Queue {
        let mut queue = Queue::new(protocol::QUEUE_MAX).unwrap();
        queue.set_size(protocol::QUEUE_MAX);
        queue.set_desc_table_address(Some(DESC as u32), Some(0));
        queue.set_avail_ring_address(Some(AVAIL as u32), Some(0));
        queue.set_used_ring_address(Some(USED as u32), Some(0));
        queue.set_ready(true);
        queue
    }

    fn push(&mut self, request: Request) -> u16 {
        self.push_raw(
            request.encode(),
            protocol::REQUEST_SIZE as u32,
            1,
            None,
            protocol::RESPONSE_SIZE as u32,
            2,
        )
    }

    fn push_raw(
        &mut self,
        bytes: [u8; protocol::REQUEST_SIZE],
        header_len: u32,
        header_flags: u16,
        response_addr: Option<u64>,
        response_len: u32,
        response_flags: u16,
    ) -> u16 {
        let slot = self.submitted;
        let addr = PAYLOAD + u64::from(slot) * 64;
        self.mem.write_slice(&bytes, GuestAddress(addr)).unwrap();
        self.mem
            .write_slice(&[0xff; protocol::RESPONSE_SIZE], GuestAddress(addr + 24))
            .unwrap();
        self.mem
            .write_obj(
                Descriptor::new(addr, header_len, header_flags, slot * 2 + 1),
                GuestAddress(DESC + u64::from(slot) * 32),
            )
            .unwrap();
        self.mem
            .write_obj(
                Descriptor::new(
                    response_addr.unwrap_or(addr + 24),
                    response_len,
                    response_flags,
                    0,
                ),
                GuestAddress(DESC + u64::from(slot) * 32 + 16),
            )
            .unwrap();
        self.mem
            .write_obj(
                (slot * 2).to_le(),
                GuestAddress(AVAIL + 4 + u64::from(slot) * 2),
            )
            .unwrap();
        self.submitted += 1;
        self.mem
            .write_obj(self.submitted.to_le(), GuestAddress(AVAIL + 2))
            .unwrap();
        slot
    }

    fn used(&self) -> u16 {
        u16::from_le(self.mem.read_obj(GuestAddress(USED + 2)).unwrap())
    }

    fn used_len(&self, slot: u16) -> u32 {
        u32::from_le(
            self.mem
                .read_obj(GuestAddress(USED + 8 + u64::from(slot) * 8))
                .unwrap(),
        )
    }

    fn response(&self, slot: u16, request: Request) -> Response {
        let mut bytes = [0; protocol::RESPONSE_SIZE];
        self.mem
            .read_slice(
                &mut bytes,
                GuestAddress(PAYLOAD + u64::from(slot) * 64 + 24),
            )
            .unwrap();
        Response::decode(&bytes, request.request_type()).unwrap()
    }
}

struct Fixture {
    vm: Arc<KvmVm>,
    geometry: MemGeometry,
    prepared: Option<MemoryService>,
    memory: Option<MemoryHandle>,
    status: WindowStatus,
    handle: MemHandle,
    worker: DeviceHandle<MemQuiesced>,
    runtime: DeviceRuntime,
    exits: DeviceExits,
    guest: Guest,
    session: Option<SessionControl>,
}

impl Fixture {
    fn new(
        context: &KvmContext,
        start: Start,
        restored: Option<(MemRestore, Vec<Range<Gpa>>)>,
    ) -> Self {
        Self::new_on(context, start, restored, Placement::Io)
    }

    fn new_on(
        context: &KvmContext,
        start: Start,
        restored: Option<(MemRestore, Vec<Range<Gpa>>)>,
        placement: Placement,
    ) -> Self {
        let boot = MemoryLayout::boot(0x10000).unwrap();
        let hot = Window::new(
            ObjectId(1),
            Range::new(ObjectOffset(0), SIZE).unwrap(),
            START,
            WindowOwner::VirtioMem,
            WindowKind::VirtioMemRam,
        )
        .unwrap();
        let layout = MemoryLayout::new(
            boot.objects()
                .map(|(id, object)| (id, *object))
                .chain([(ObjectId(1), Object { capacity: SIZE })]),
            boot.windows()
                .map(|(id, window)| (id, *window))
                .chain([(WindowId(1), hot)]),
        )
        .unwrap();
        let policy = HostPolicy {
            kind: BackingKind::AnonymousPrivate,
            page_size: BackingPageSize::base(context.capabilities().page_size),
        };
        let geometry =
            MemGeometry::new(&layout, WindowId(1), BLOCK, CHUNK, policy.page_size).unwrap();
        let store = MemoryStore::materialize(layout, policy, context.capabilities()).unwrap();
        let (vm, mut slots) = KvmVm::new(context, store).unwrap();
        let permits = slots.allocate(geometry.chunk_count() as usize).unwrap();
        let revision = restored
            .as_ref()
            .map_or(0, |(seed, _)| seed.state.memory_revision);
        let restored_queue = restored.as_ref().and_then(|(seed, _)| seed.state.queue);
        let population = match &restored {
            Some((_, ranges)) => Population::restore(geometry, ranges.iter().copied()).unwrap(),
            None => Population::new(geometry),
        };
        let (service, window, status) =
            MemoryService::prepare_restored(Arc::clone(&vm), population, revision, permits)
                .unwrap();
        let (resources, handle) = match restored {
            Some((seed, _)) => {
                MemResources::restore(geometry, window, status.clone(), seed).unwrap()
            }
            None => MemResources::new(geometry, window, status.clone()).unwrap(),
        };
        let mut guest = Guest {
            mem: vm.memory().view(),
            submitted: 0,
        };
        let session = restored_queue.map(|queue| {
            guest.submitted = queue.next_avail;
            guest
                .mem
                .write_obj(queue.next_avail.to_le(), GuestAddress(AVAIL + 2))
                .unwrap();
            guest
                .mem
                .write_obj(queue.next_used.to_le(), GuestAddress(USED + 2))
                .unwrap();
            let (control, session) = session_channel();
            handle
                .activator
                .activate(Activation {
                    queues: vec![Queue::try_from(queue).unwrap()],
                    mem: Arc::clone(&guest.mem),
                    interrupt: Arc::clone(&handle.interrupt),
                    driver_features: protocol::REQUIRED_FEATURES,
                    session,
                })
                .unwrap();
            control
        });
        let (mut runtime, exits) = DeviceRuntime::new();
        let worker = runtime
            .spawn_on("virtio-mem", placement, start, move |control| async move {
                resources.into_device()?.run(control).await
            })
            .unwrap();
        Self {
            vm,
            geometry,
            prepared: Some(service),
            memory: None,
            status,
            handle,
            worker,
            runtime,
            exits,
            guest,
            session,
        }
    }

    fn start_service(&mut self) {
        self.memory = Some(
            self.prepared
                .take()
                .unwrap()
                .spawn(&mut self.runtime, Start::Running)
                .unwrap(),
        );
    }

    fn activate_queue(&mut self, queue: Queue, features: u64) {
        let (control, session) = session_channel();
        self.handle
            .activator
            .activate(Activation {
                queues: vec![queue],
                mem: Arc::clone(&self.guest.mem),
                interrupt: Arc::clone(&self.handle.interrupt),
                driver_features: features,
                session,
            })
            .unwrap();
        self.session = Some(control);
        self.handle.queue_notifier.write(1).unwrap();
    }

    fn activate(&mut self) {
        self.activate_queue(self.guest.queue(), protocol::REQUIRED_FEATURES);
    }

    fn config(&self) -> Config {
        Config::decode(&self.handle.config.snapshot().bytes).unwrap()
    }

    async fn request(&mut self, request: Request) -> Response {
        let slot = self.guest.push(request);
        self.handle.queue_notifier.write(1).unwrap();
        wait(|| self.guest.used() == slot + 1).await;
        assert_eq!(self.guest.used_len(slot), protocol::RESPONSE_SIZE as u32);
        self.guest.response(slot, request)
    }

    async fn quiesce(&self) -> MemQuiesced {
        let ControlResponse::Replied(capture) = self.worker.quiesce(WITHIN).await else {
            panic!("worker did not quiesce")
        };
        capture
    }

    async fn finish(mut self) {
        self.worker.stop();
        self.worker.finished().await;
        // The integration owner stops the service only after worker queue effects settle.
        if let Some(memory) = self.memory.take() {
            memory.stop();
            memory.finished().await;
        } else {
            self.prepared
                .take()
                .unwrap()
                .into_resources()
                .release()
                .unwrap();
        }
        self.runtime.shutdown_wait(WITHIN).await.unwrap();
        assert!(
            self.vm
                .memory()
                .layout()
                .window(self.geometry.window_id())
                .is_some()
        );
    }
}

#[tokio::test(flavor = "local")]
async fn library_boundary_plug_duplicate_nack_mixed_state_and_unplug_all() {
    let Some(context) = kvm() else { return };
    let mut fixture = Fixture::new(&context, Start::Running, None);
    fixture.start_service();
    assert_eq!(
        fixture.handle.desc.required_features,
        protocol::REQUIRED_FEATURES
    );
    assert_eq!(fixture.handle.desc.device_type, 24);
    assert_eq!(
        fixture.handle.desc.config,
        baseline(fixture.geometry).encode()
    );
    fixture
        .handle
        .resize
        .send(2 * BLOCK)
        .unwrap()
        .await
        .unwrap();
    fixture.handle.interrupt.acknowledge(u32::MAX);
    fixture.activate();

    let plug = Request::Plug {
        addr: START,
        blocks: 1,
    };
    assert_eq!(fixture.request(plug).await, Response::Ack);
    assert_eq!(fixture.config().plugged_size, BLOCK);
    assert_eq!(fixture.status.snapshot().revision, 1);
    assert_eq!(fixture.handle.interrupt.status(), 1);
    let generation = fixture.handle.config.generation();
    assert_eq!(fixture.request(plug).await, Response::Error);
    assert_eq!(
        fixture
            .request(Request::Plug {
                addr: range(1, 1).start(),
                blocks: 2
            })
            .await,
        Response::Nack
    );
    assert_eq!(fixture.handle.config.generation(), generation);
    assert_eq!(fixture.status.snapshot().revision, 1);
    assert_eq!(
        fixture
            .request(Request::State {
                addr: START,
                blocks: 2
            })
            .await,
        Response::State(BlockState::Mixed)
    );
    assert_eq!(
        fixture
            .request(Request::State {
                addr: range(4, 1).start(),
                blocks: 1
            })
            .await,
        Response::Error
    );
    assert_eq!(fixture.handle.config.generation(), generation);
    assert_eq!(
        fixture
            .request(Request::Unplug {
                addr: START,
                blocks: 1
            })
            .await,
        Response::Ack
    );
    assert_eq!(fixture.config().plugged_size, 0);

    fixture.handle.resize.send(0).unwrap().await.unwrap();
    fixture.handle.interrupt.acknowledge(u32::MAX);
    let mut ignored = Request::UnplugAll.encode();
    ignored[2..].fill(0xff);
    let slot = fixture.guest.push_raw(ignored, 24, 1, None, 10, 2);
    fixture.handle.queue_notifier.write(1).unwrap();
    wait(|| fixture.guest.used() == slot + 1).await;
    assert_eq!(
        fixture.guest.response(slot, Request::UnplugAll),
        Response::Ack
    );
    assert_eq!(fixture.config().usable_region_size, 0);
    assert_eq!(fixture.handle.interrupt.status(), 1); // No mandatory config IRQ on ALL shrink.
    fixture.finish().await;
}

#[tokio::test(flavor = "local")]
async fn mmio_negotiation_notify_config_and_reset_reach_the_kvm_memory_service() {
    let Some(context) = kvm() else { return };
    let mut fixture = Fixture::new(&context, Start::Running, None);
    fixture.start_service();
    fixture.handle.resize.send(BLOCK).unwrap().await.unwrap();
    let transport = MmioTransport::new(
        fixture.handle.desc.clone(),
        fixture.handle.config.clone(),
        Arc::clone(&fixture.guest.mem),
        fixture.handle.activator.clone(),
        Arc::clone(&fixture.handle.interrupt),
        vec![fixture.handle.queue_notifier.try_clone().unwrap()],
    )
    .unwrap();
    let write = |offset, value: u32| transport.write(offset, &value.to_le_bytes());
    write(0x70, 1);
    write(0x70, 3);
    write(0x24, 0);
    write(0x20, protocol::REQUIRED_FEATURES as u32);
    write(0x24, 1);
    write(0x20, (protocol::REQUIRED_FEATURES >> 32) as u32);
    write(0x70, 11);
    write(0x30, 0);
    write(0x38, u32::from(protocol::QUEUE_MAX));
    for (register, address) in [(0x80, DESC), (0x90, AVAIL), (0xa0, USED)] {
        write(register, address as u32);
        write(register + 4, 0);
    }
    write(0x44, 1);
    write(0x70, 15);
    let request = Request::Plug {
        addr: START,
        blocks: 1,
    };
    fixture.guest.push(request);
    write(0x50, 0);
    wait(|| fixture.guest.used() == 1).await;
    assert_eq!(fixture.guest.response(0, request), Response::Ack);
    assert_eq!(fixture.status.snapshot().population.plugged_size(), BLOCK);
    let mut plugged = [0; 8];
    transport.read(
        0x100 + protocol::CONFIG_PLUGGED_SIZE_OFFSET as u64,
        &mut plugged,
    );
    assert_eq!(u64::from_le_bytes(plugged), BLOCK);
    let capture = fixture.quiesce().await;
    assert!(capture.queue.is_some());
    write(0x70, 0); // Synchronous MMIO reset waits on the separate I/O-runtime worker.
    assert_eq!(transport.snapshot().regs.status, 0);
    assert_eq!(fixture.quiesce().await.queue, None);
    assert_eq!(fixture.config().plugged_size, BLOCK); // Reset retains population/config.
    drop(transport);
    fixture.finish().await;
}

#[tokio::test(flavor = "local")]
async fn invalid_available_index_exits_without_spinning_or_memory_effects() {
    let Some(context) = kvm() else { return };
    let mut fixture = Fixture::new(&context, Start::Running, None);
    fixture
        .guest
        .mem
        .write_obj((protocol::QUEUE_MAX + 1).to_le(), GuestAddress(AVAIL + 2))
        .unwrap();
    fixture.activate();
    tokio::time::timeout(WITHIN, fixture.worker.finished())
        .await
        .unwrap();
    let exit = fixture.exits.next().await.unwrap();
    let Err(DeviceFailure::Failed(error)) = exit.result else {
        panic!("expected queue failure")
    };
    assert!(matches!(
        error.downcast_ref::<MemError>(),
        Some(MemError::AvailableIndex)
    ));
    assert_eq!(fixture.status.snapshot().revision, 0);
    assert_eq!(fixture.guest.used(), 0);
    fixture.finish().await;
}

#[tokio::test(flavor = "local")]
async fn unchanged_resize_and_service_phases_do_not_generate_config_interrupts() {
    let Some(context) = kvm() else { return };
    let mut fixture = Fixture::new(&context, Start::Running, None);
    fixture.start_service();
    fixture.handle.resize.send(BLOCK).unwrap().await.unwrap();
    assert_eq!(fixture.config().usable_region_size, CHUNK);
    assert_eq!(fixture.handle.config.generation(), 1);
    assert_eq!(fixture.handle.interrupt.status(), 2);
    fixture.handle.interrupt.acknowledge(u32::MAX);
    fixture.handle.resize.send(BLOCK).unwrap().await.unwrap();
    assert_eq!(fixture.handle.config.generation(), 1);
    assert_eq!(fixture.handle.interrupt.status(), 0);
    assert!(matches!(
        fixture.handle.resize.send(1).unwrap().await,
        Err(ResizeError::Config(protocol::ConfigError::Unaligned { .. }))
    ));
    assert!(matches!(
        fixture.handle.resize.send(SIZE + BLOCK).unwrap().await,
        Err(ResizeError::Config(
            protocol::ConfigError::TargetExceedsRegion { .. }
        ))
    ));
    let capture = fixture.quiesce().await;
    assert!(matches!(
        fixture.handle.resize.send(0),
        Err(ResizeError::Paused)
    ));
    assert_eq!(fixture.quiesce().await, capture);
    assert!(matches!(
        fixture.memory.as_ref().unwrap().quiesce(WITHIN).await,
        ControlResponse::Replied(_)
    ));
    assert_eq!(
        fixture.memory.as_ref().unwrap().resume(WITHIN).await,
        ControlResponse::Replied(())
    );
    assert_eq!(fixture.quiesce().await, capture);
    assert_eq!(fixture.handle.config.generation(), 1);
    assert_eq!(fixture.handle.interrupt.status(), 0);
    fixture.finish().await;
}

#[tokio::test(flavor = "local")]
async fn parked_activation_consumes_queue_but_performs_no_guest_io_or_resize() {
    let Some(context) = kvm() else { return };
    let mut fixture = Fixture::new(&context, Start::Parked, None);
    fixture.start_service();
    let resize = fixture.handle.resize.send(BLOCK);
    fixture.guest.push(Request::State {
        addr: START,
        blocks: 1,
    });
    fixture.activate();
    let capture = fixture.quiesce().await;
    assert_eq!(capture.queue.unwrap().next_avail, 0);
    assert_eq!(capture.queue.unwrap().next_used, 0);
    let result = match resize {
        Ok(reply) => reply.await,
        Err(error) => Err(error),
    };
    assert!(matches!(result, Err(ResizeError::Paused)));
    assert_eq!(fixture.guest.used(), 0);
    assert_eq!(fixture.handle.config.generation(), 0);
    assert_eq!(fixture.handle.interrupt.status(), 0);
    assert_eq!(fixture.quiesce().await, capture);
    assert_eq!(
        fixture.worker.resume(WITHIN).await,
        ControlResponse::Replied(())
    );
    wait(|| fixture.guest.used() == 1).await;
    assert_eq!(
        fixture.guest.response(
            0,
            Request::State {
                addr: START,
                blocks: 1
            }
        ),
        Response::Error
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "local")]
async fn reset_and_quiesce_keep_pending_memory_and_cancelled_accepted_resize_until_publication() {
    let Some(context) = kvm() else { return };
    let mut fixture = Fixture::new(&context, Start::Running, None);
    // Leave the prepared service unspawned to hold the worker's accepted operation.
    fixture.handle.resize.send(BLOCK).unwrap().await.unwrap();
    fixture.guest.push(Request::Plug {
        addr: START,
        blocks: 1,
    });
    fixture.guest.push(Request::Plug {
        addr: range(1, 1).start(),
        blocks: 1,
    });
    fixture.activate();
    wait(|| {
        fixture
            .guest
            .mem
            .read_obj::<u16>(GuestAddress(USED))
            .unwrap()
            == 1
    })
    .await;
    assert_eq!(fixture.guest.used(), 0);
    let resize = fixture.handle.resize.send(0).unwrap();
    wait(|| fixture.handle.resize.resize.capacity() == HOST_QUEUE_SIZE).await;
    drop(resize); // Completion cancellation cannot cancel the owned envelope update.
    assert_eq!(
        fixture.worker.quiesce(Duration::from_millis(10)).await,
        ControlResponse::TimedOut
    );
    assert!(matches!(
        fixture
            .handle
            .resize
            .retry(MemoryOperationId(1))
            .unwrap()
            .await,
        Err(ResizeError::Retry(RetryError::NotStalled))
    ));
    assert_eq!(
        fixture.session.as_mut().unwrap().reset(Duration::ZERO),
        Err(ResetError::TimedOut)
    );
    assert!(!fixture.worker.has_exited());
    assert_eq!(fixture.config().plugged_size, 0);
    fixture.start_service();
    let capture = fixture.quiesce().await;
    assert_eq!(capture.queue, None); // Never resurrect the reset queue on Quiesce.
    assert_eq!(capture.memory_revision, 1);
    assert_eq!(capture.requested_size, 0);
    assert_eq!(capture.usable_region_size, CHUNK);
    assert_eq!(fixture.config().plugged_size, BLOCK);
    assert_eq!(fixture.guest.used(), 1); // The second available chain was not accepted.
    assert_eq!(
        fixture.guest.response(
            0,
            Request::Plug {
                addr: START,
                blocks: 1
            }
        ),
        Response::Ack
    );
    assert_eq!(
        fixture.session.as_mut().unwrap().reset(Duration::ZERO),
        Ok(())
    );
    assert_eq!(fixture.quiesce().await, capture);
    fixture.finish().await;
}

#[tokio::test(flavor = "local")]
async fn stop_waits_for_pending_memory_before_releasing_queue_and_service_owner() {
    let Some(context) = kvm() else { return };
    let mut fixture = Fixture::new(&context, Start::Running, None);
    fixture.handle.resize.send(BLOCK).unwrap().await.unwrap();
    fixture.guest.push(Request::Plug {
        addr: START,
        blocks: 1,
    });
    fixture.activate();
    wait(|| {
        fixture
            .guest
            .mem
            .read_obj::<u16>(GuestAddress(USED))
            .unwrap()
            == 1
    })
    .await;
    fixture.worker.stop();
    assert_eq!(
        fixture.session.as_mut().unwrap().reset(Duration::ZERO),
        Err(ResetError::TimedOut)
    );
    assert!(!fixture.worker.has_exited());
    fixture.start_service();
    fixture.worker.finished().await;
    assert_eq!(fixture.guest.used(), 1);
    assert_eq!(fixture.config().plugged_size, BLOCK);
    assert_eq!(
        fixture.session.as_mut().unwrap().reset(Duration::ZERO),
        Ok(())
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "local")]
async fn invalid_descriptor_directions_lengths_bounds_and_overflow_have_no_memory_effects() {
    let Some(context) = kvm() else { return };
    let mut fixture = Fixture::new(&context, Start::Running, None);
    fixture.start_service();
    fixture.handle.resize.send(SIZE).unwrap().await.unwrap();
    fixture.activate();
    for (header_len, header_flags, addr, response_len, response_flags) in [
        (23, 1, None, 10, 2),
        (24, 3, None, 10, 2),
        (24, 1, None, 9, 2),
        (24, 1, None, 10, 0),
        (24, 1, Some(u64::MAX - 4), 10, 2),
        (u32::MAX, 1, None, 10, 2),
        (24, 1, None, u32::MAX, 2),
        (24, 0, None, 10, 2),
        (24, 1, None, 10, 3),
    ] {
        let slot = fixture.guest.push_raw(
            Request::Plug {
                addr: START,
                blocks: 1,
            }
            .encode(),
            header_len,
            header_flags,
            addr,
            response_len,
            response_flags,
        );
        fixture.handle.queue_notifier.write(1).unwrap();
        wait(|| fixture.guest.used() == slot + 1).await;
        assert_eq!(fixture.guest.used_len(slot), 0);
        assert_eq!(fixture.status.snapshot().population.plugged_size(), 0);
        assert_eq!(fixture.status.snapshot().revision, 0);
    }
    assert_eq!(fixture.handle.config.generation(), 1);
    fixture.finish().await;
}

#[tokio::test(flavor = "local")]
async fn revision_exhaustion_is_a_typed_failure_never_a_guest_ack() {
    let Some(context) = kvm() else { return };
    let config = Config {
        block_size: BLOCK,
        node_id: 0,
        addr: START,
        region_size: SIZE,
        usable_region_size: CHUNK,
        plugged_size: 0,
        requested_size: BLOCK,
    };
    let seed = MemRestore {
        state: MemQuiesced {
            requested_size: BLOCK,
            usable_region_size: CHUNK,
            memory_revision: u64::MAX,
            queue: None,
        },
        config: ConfigSnapshot {
            bytes: config.encode().to_vec(),
            generation: 7,
        },
    };
    let mut fixture = Fixture::new(&context, Start::Running, Some((seed, vec![])));
    fixture.start_service();
    fixture.guest.push(Request::Plug {
        addr: START,
        blocks: 1,
    });
    fixture.activate();
    tokio::time::timeout(WITHIN, fixture.worker.finished())
        .await
        .unwrap();
    let exit = fixture.exits.next().await.unwrap();
    let Err(DeviceFailure::Failed(error)) = exit.result else {
        panic!("expected revision failure")
    };
    assert!(matches!(
        error.downcast_ref::<MemError>(),
        Some(MemError::Operation(MemoryOperationError::RevisionExhausted))
    ));
    assert_eq!(fixture.guest.used(), 0);
    assert_eq!(fixture.status.snapshot().population.plugged_size(), 0);
    assert_eq!(fixture.handle.config.generation(), 7);
    fixture.finish().await;
}

#[tokio::test(flavor = "local")]
async fn worker_revalidates_required_features_even_for_direct_library_activation() {
    let Some(context) = kvm() else { return };
    for features in [
        0,
        protocol::VERSION_1,
        protocol::REQUIRED_FEATURES | (1 << 28),
    ] {
        let mut fixture = Fixture::new(&context, Start::Parked, None);
        fixture.activate_queue(fixture.guest.queue(), features);
        fixture.worker.finished().await;
        let exit = fixture.exits.next().await.unwrap();
        let Err(DeviceFailure::Failed(error)) = exit.result else {
            panic!("expected typed worker failure")
        };
        assert!(matches!(
            error.downcast_ref::<MemError>(),
            Some(MemError::Features(_))
        ));
        assert_eq!(fixture.guest.used(), 0);
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "local")]
async fn invalid_restored_ring_cursors_reject_before_controls_resize_or_guest_io() {
    let Some(context) = kvm() else { return };
    for start in [Start::Parked, Start::Running] {
        for (next_avail, next_used, used, available, oversized) in [
            (1, 0, 0u16, 2, false),
            (1, 1, 0, 2, false),
            (1, 1, 2, 2, false),
            (1, 1, 1, protocol::QUEUE_MAX + 2, true),
            (7, 7, 7, 6, true),
        ] {
            let seed = restore_seed(next_avail, next_used);
            let mut fixture = Fixture::new_on(
                &context,
                start,
                Some((seed.clone(), vec![])),
                Placement::VmmMain,
            );
            fixture.guest.push(Request::Plug {
                addr: START,
                blocks: 1,
            });
            fixture
                .guest
                .mem
                .write_obj(used.to_le(), GuestAddress(USED + 2))
                .unwrap();
            fixture
                .guest
                .mem
                .write_obj(available.to_le(), GuestAddress(AVAIL + 2))
                .unwrap();
            let mut before = vec![0; 0x10000];
            fixture
                .guest
                .mem
                .read_slice(&mut before, GuestAddress(0))
                .unwrap();
            let resize = fixture.handle.resize.send(2 * BLOCK).unwrap();
            let resume = enqueue(fixture.worker.resume(WITHIN));
            let quiesce = enqueue(fixture.worker.quiesce(WITHIN));

            tokio::time::timeout(WITHIN, fixture.worker.finished())
                .await
                .unwrap();
            assert_eq!(resume.await, ControlResponse::Exited);
            assert_eq!(quiesce.await, ControlResponse::Exited);
            assert!(matches!(resize.await, Err(ResizeError::Stopped)));
            let exit = fixture.exits.next().await.unwrap();
            let Err(DeviceFailure::Failed(error)) = exit.result else {
                panic!("expected restored cursor failure")
            };
            if oversized {
                assert!(matches!(
                    error.downcast_ref::<MemError>(),
                    Some(MemError::AvailableIndex)
                ));
            } else {
                assert!(matches!(
                    error.downcast_ref::<MemError>(),
                    Some(MemError::QueuePosition)
                ));
            }
            let mut after = vec![0; before.len()];
            fixture
                .guest
                .mem
                .read_slice(&mut after, GuestAddress(0))
                .unwrap();
            assert_eq!(after, before);
            assert_eq!(fixture.status.snapshot().revision, 0);
            assert_eq!(fixture.status.snapshot().population.plugged_size(), 0);
            assert_eq!(fixture.handle.config.snapshot(), seed.config);
            assert_eq!(fixture.handle.interrupt.status(), 0);
            fixture.finish().await;
        }
    }
}

#[tokio::test(flavor = "local")]
async fn restored_nonzero_and_wrapped_cursors_accept_available_work_only_after_resume() {
    let Some(context) = kvm() else { return };
    for cursor in [0, 7, u16::MAX] {
        let seed = restore_seed(cursor, cursor);
        let mut fixture = Fixture::new_on(
            &context,
            Start::Parked,
            Some((seed.clone(), vec![])),
            Placement::VmmMain,
        );
        let request = Request::State {
            addr: START,
            blocks: 1,
        };
        fixture.guest.submitted = 0;
        fixture.guest.push(request);
        fixture
            .guest
            .mem
            .write_obj(
                0u16,
                GuestAddress(AVAIL + 4 + u64::from(cursor % protocol::QUEUE_MAX) * 2),
            )
            .unwrap();
        fixture
            .guest
            .mem
            .write_obj(cursor.wrapping_add(1).to_le(), GuestAddress(AVAIL + 2))
            .unwrap();
        assert_eq!(fixture.quiesce().await, seed.state);
        assert_eq!(fixture.guest.used(), cursor);
        assert_eq!(fixture.handle.interrupt.status(), 0);
        assert_eq!(
            fixture.worker.resume(WITHIN).await,
            ControlResponse::Replied(())
        );
        wait(|| fixture.guest.used() == cursor.wrapping_add(1)).await;
        assert_eq!(
            fixture.guest.response(0, request),
            Response::State(BlockState::Unplugged)
        );
        assert_eq!(
            fixture.guest.used_len(cursor % protocol::QUEUE_MAX),
            protocol::RESPONSE_SIZE as u32
        );
        assert_eq!(fixture.handle.config.snapshot(), seed.config);
        assert_eq!(fixture.status.snapshot().revision, 0);
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "local")]
async fn cold_activation_rejects_nonzero_guest_or_backend_used_cursors() {
    let Some(context) = kvm() else { return };
    for backend in [0, 1] {
        let mut fixture = Fixture::new_on(&context, Start::Parked, None, Placement::VmmMain);
        let mut queue = fixture.guest.queue();
        queue.set_next_avail(backend);
        queue.set_next_used(backend);
        fixture
            .guest
            .mem
            .write_obj(1u16.to_le(), GuestAddress(USED + 2))
            .unwrap();
        fixture
            .guest
            .mem
            .write_obj(backend.to_le(), GuestAddress(AVAIL + 2))
            .unwrap();
        fixture.activate_queue(queue, protocol::REQUIRED_FEATURES);
        tokio::time::timeout(WITHIN, fixture.worker.finished())
            .await
            .unwrap();
        let exit = fixture.exits.next().await.unwrap();
        let Err(DeviceFailure::Failed(error)) = exit.result else {
            panic!("expected cold cursor failure")
        };
        assert!(matches!(
            error.downcast_ref::<MemError>(),
            Some(MemError::QueuePosition)
        ));
        assert_eq!(fixture.guest.used(), 1);
        assert_eq!(fixture.status.snapshot().revision, 0);
        assert_eq!(fixture.handle.config.generation(), 0);
        assert_eq!(fixture.handle.interrupt.status(), 0);
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "local")]
async fn more_than_sixteen_pending_quiesces_and_resumes_keep_every_reply_alive() {
    let Some(context) = kvm() else { return };
    let mut fixture = Fixture::new_on(&context, Start::Running, None, Placement::VmmMain);
    fixture.handle.resize.send(BLOCK).unwrap().await.unwrap();
    fixture.guest.push(Request::Plug {
        addr: START,
        blocks: 1,
    });
    fixture.activate();
    wait(|| {
        fixture
            .guest
            .mem
            .read_obj::<u16>(GuestAddress(USED))
            .unwrap()
            == 1
    })
    .await;
    let mut quiesces = (0..32)
        .map(|_| enqueue(fixture.worker.quiesce(WITHIN)))
        .collect::<Vec<_>>();
    let mut resumes = (0..32)
        .map(|_| enqueue(fixture.worker.resume(WITHIN)))
        .collect::<Vec<_>>();
    // Biased control polling consumes the entire prefix before forwarding this retry.
    assert!(matches!(
        fixture
            .handle
            .resize
            .retry(MemoryOperationId(1))
            .unwrap()
            .await,
        Err(ResizeError::Retry(RetryError::NotStalled))
    ));
    for reply in &mut quiesces {
        assert_pending(reply.as_mut());
    }
    for reply in &mut resumes {
        assert_pending(reply.as_mut());
    }
    assert!(!fixture.worker.has_exited());
    assert_eq!(fixture.guest.used(), 0);
    fixture.memory = Some(
        fixture
            .prepared
            .take()
            .unwrap()
            .spawn(&mut fixture.runtime, Start::Running)
            .unwrap(),
    );
    for reply in quiesces {
        let ControlResponse::Replied(capture) = reply.await else {
            panic!("live worker lost a quiesce reply")
        };
        assert_eq!(capture.memory_revision, 1);
        assert_eq!(capture.queue.unwrap().next_avail, 1);
        assert_eq!(capture.queue.unwrap().next_used, 1);
    }
    for reply in resumes {
        assert_eq!(reply.await, ControlResponse::Replied(()));
    }
    assert!(!fixture.worker.has_exited());
    assert_eq!(fixture.guest.used(), 1);
    fixture.finish().await;
}

#[tokio::test(flavor = "local")]
async fn overlapping_quiesce_resume_quiesce_preserves_fifo_and_the_final_park() {
    let Some(context) = kvm() else { return };
    let mut fixture = Fixture::new_on(&context, Start::Running, None, Placement::VmmMain);
    fixture
        .handle
        .resize
        .send(2 * BLOCK)
        .unwrap()
        .await
        .unwrap();
    for addr in [START, range(1, 1).start()] {
        fixture.guest.push(Request::Plug { addr, blocks: 1 });
    }
    fixture.activate();
    wait(|| {
        fixture
            .guest
            .mem
            .read_obj::<u16>(GuestAddress(USED))
            .unwrap()
            == 1
    })
    .await;
    let first = enqueue(fixture.worker.quiesce(WITHIN));
    let resume = enqueue(fixture.worker.resume(WITHIN));
    let last = enqueue(fixture.worker.quiesce(WITHIN));
    assert!(matches!(
        fixture
            .handle
            .resize
            .retry(MemoryOperationId(1))
            .unwrap()
            .await,
        Err(ResizeError::Retry(RetryError::NotStalled))
    ));
    assert_eq!(fixture.guest.used(), 0);
    fixture.memory = Some(
        fixture
            .prepared
            .take()
            .unwrap()
            .spawn(&mut fixture.runtime, Start::Running)
            .unwrap(),
    );
    let ControlResponse::Replied(first) = first.await else {
        panic!("first quiesce did not reply")
    };
    assert_eq!(resume.await, ControlResponse::Replied(()));
    let ControlResponse::Replied(last) = last.await else {
        panic!("last quiesce did not reply")
    };
    assert_eq!(last, first);
    assert_eq!(last.memory_revision, 1);
    assert_eq!(last.queue.unwrap().next_avail, 1);
    assert_eq!(last.queue.unwrap().next_used, 1);
    assert_eq!(fixture.guest.used(), 1);
    assert_eq!(fixture.status.snapshot().population.plugged_size(), BLOCK);
    assert!(matches!(
        fixture.handle.resize.send(0),
        Err(ResizeError::Paused)
    ));
    assert_eq!(fixture.quiesce().await, last);
    assert_eq!(
        fixture.worker.resume(WITHIN).await,
        ControlResponse::Replied(())
    );
    wait(|| fixture.guest.used() == 2).await;
    assert_eq!(fixture.status.snapshot().revision, 2);
    fixture.finish().await;
}

#[tokio::test(flavor = "local")]
async fn initial_resume_without_activation_does_not_deadlock() {
    let Some(context) = kvm() else { return };
    for start in [Start::Parked, Start::Running] {
        let fixture = Fixture::new_on(&context, start, None, Placement::VmmMain);
        assert_eq!(
            fixture.worker.resume(WITHIN).await,
            ControlResponse::Replied(())
        );
        assert_eq!(fixture.quiesce().await.queue, None);
        assert!(!fixture.worker.has_exited());
        assert_eq!(fixture.status.snapshot().revision, 0);
        assert_eq!(fixture.handle.config.generation(), 0);
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "local")]
async fn restored_config_generation_identity_revision_and_active_backend_queue_are_replayed() {
    let Some(context) = kvm() else { return };
    let mut original = Fixture::new(&context, Start::Running, None);
    original.start_service();
    original.handle.resize.send(BLOCK).unwrap().await.unwrap();
    original.activate();
    assert_eq!(
        original
            .request(Request::Plug {
                addr: START,
                blocks: 1
            })
            .await,
        Response::Ack
    );
    let state = original.quiesce().await;
    let config = original.handle.config.snapshot();
    let desc = original.handle.desc.clone();
    let plugged = original
        .status
        .snapshot()
        .population
        .plugged_ranges()
        .collect::<Vec<_>>();
    original.finish().await;

    let mut restored = Fixture::new(
        &context,
        Start::Parked,
        Some((
            MemRestore {
                state,
                config: config.clone(),
            },
            plugged,
        )),
    );
    restored.start_service();
    assert_eq!(restored.handle.desc, desc);
    assert_eq!(restored.handle.config.snapshot(), config);
    // The fixture restored ring bytes and queued Activation before spawning parked.
    let state_request = Request::State {
        addr: START,
        blocks: 1,
    };
    restored.guest.push(state_request);
    let transport = MmioTransport::new(
        restored.handle.desc.clone(),
        restored.handle.config.clone(),
        Arc::clone(&restored.guest.mem),
        restored.handle.activator.clone(),
        Arc::clone(&restored.handle.interrupt),
        vec![restored.handle.queue_notifier.try_clone().unwrap()],
    )
    .unwrap();
    // Construction does not derive backend activity from this cold transport.
    assert_eq!(restored.quiesce().await, state);
    assert_eq!(restored.guest.used(), 1);
    assert_eq!(restored.handle.interrupt.status(), 0);
    assert_eq!(
        restored.worker.resume(WITHIN).await,
        ControlResponse::Replied(())
    );
    wait(|| restored.guest.used() == 2).await;
    assert_eq!(
        restored.guest.response(1, state_request),
        Response::State(BlockState::Plugged)
    );
    assert_eq!(restored.handle.config.snapshot(), config);
    assert_eq!(restored.status.snapshot().revision, state.memory_revision);
    drop(transport);
    restored.finish().await;
}
