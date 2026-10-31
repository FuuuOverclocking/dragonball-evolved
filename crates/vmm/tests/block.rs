mod common;

use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use common::guest::*;
use common::{TempDisk, wait_until};
use virtio_queue::QueueT;
use vm_memory::{Bytes, GuestAddress};
use vmm::bus::BusDevice;
use vmm::devices::async_fd::AsyncEventFd;
use vmm::devices::runtime::{DeviceExits, DeviceHandle, DeviceRuntime, Placement, Response, Start};
use vmm::devices::virtio::block::{BlockError, BlockHandle, BlockQuiesced, BlockResources};
use vmm::devices::virtio::mmio::{MmioTransport, NEEDS_RESET};
use vmm::devices::virtio::{
    Activation, ActivationError, ResetError, SessionControl, activation_channel, session_channel,
};

const WITHIN: Duration = Duration::from_secs(5);

struct Block {
    runtime: DeviceRuntime,
    exits: DeviceExits,
    control: DeviceHandle<BlockQuiesced>,
    handle: BlockHandle,
}

impl Block {
    fn new(
        disk: &TempDisk,
        read_only: bool,
        placement: Placement,
        start: Start,
        iops: Option<(u64, Duration)>,
    ) -> Self {
        let (resources, handle) =
            BlockResources::open("my-disk", disk.path(), read_only, iops).unwrap();
        let (mut runtime, exits) = DeviceRuntime::new();
        let control = runtime
            .spawn_on("block", placement, start, move |control| async move {
                resources.into_device()?.run(control).await
            })
            .unwrap();
        Self {
            runtime,
            exits,
            control,
            handle,
        }
    }

    async fn complete(&self, guest: &GuestDriver, count: u16) {
        self.handle.queue_notifier.write(1).unwrap();
        wait_until("block completion", || guest.used_idx() == count).await;
    }

    async fn shutdown(self) {
        self.runtime.shutdown(WITHIN).await.unwrap();
    }

    fn transport(&self, guest: &GuestDriver) -> Arc<MmioTransport> {
        Arc::new(MmioTransport::new(
            self.handle.desc.clone(),
            guest.mem.clone(),
            self.handle.activator.clone(),
            self.handle.interrupt.clone(),
            vec![self.handle.queue_notifier.try_clone().unwrap()],
        ))
    }
}

fn activate(handle: &BlockHandle, guest: &GuestDriver) -> SessionControl {
    let (control, session) = session_channel();
    let activation = Activation {
        mem: Arc::clone(&guest.mem),
        queues: vec![guest.queue()],
        interrupt: Arc::clone(&handle.interrupt),
        driver_features: 1 << 32,
        session,
    };
    let activator = handle.activator.clone();
    std::thread::spawn(move || activator.activate(activation).unwrap())
        .join()
        .unwrap();
    control
}

fn request(
    guest: &mut GuestDriver,
    kind: u32,
    sector: u64,
    len: u32,
) -> (GuestAddress, GuestAddress, u16) {
    let header = guest.request_header(kind, sector);
    let data = guest.alloc(len);
    let status = guest.alloc(1);
    guest.write_bytes(status, &[0xff]);
    let payload = if kind == VIRTIO_BLK_T_OUT {
        Segment::readable(data, len)
    } else {
        Segment::writable(data, len)
    };
    let head = guest.submit(&[
        Segment::readable(header, 16),
        payload,
        Segment::writable(status, 1),
    ]);
    (data, status, head)
}

fn configure(transport: &MmioTransport, used: u32, size: u32) {
    for (offset, value) in [
        (0x70, 1u32),
        (0x70, 3),
        (0x24, 1),
        (0x20, 1),
        (0x70, 11),
        (0x30, 0),
        (0x38, size),
        (0x80, DESC_TABLE as u32),
        (0x90, AVAIL_RING as u32),
        (0xa0, used),
        (0x44, 1),
        (0x70, 15),
    ] {
        transport.write(offset, &value.to_le_bytes());
    }
}

fn reg(transport: &MmioTransport, offset: u64) -> u32 {
    let mut data = [0; 4];
    transport.read(offset, &mut data);
    u32::from_le_bytes(data)
}

async fn reset(transport: Arc<MmioTransport>) {
    tokio::time::timeout(
        WITHIN + Duration::from_secs(1),
        tokio::task::spawn_blocking(move || transport.write(0x70, &0u32.to_le_bytes())),
    )
    .await
    .unwrap()
    .unwrap();
}

async fn reset_session(mut session: SessionControl) -> Result<(), ResetError> {
    tokio::task::spawn_blocking(move || session.reset(WITHIN))
        .await
        .unwrap()
}

#[tokio::test(flavor = "local")]
async fn read_request_moves_the_correct_sector_and_interrupts() {
    let mut contents = vec![0xab; SECTOR_SIZE];
    contents.extend_from_slice(&[0xcd; SECTOR_SIZE]);
    let disk = TempDisk::new("read", &contents);
    let block = Block::new(&disk, false, Placement::VmmMain, Start::Running, None);
    let irq =
        AsyncEventFd::from_event_fd(block.handle.interrupt.irq_fd().try_clone().unwrap()).unwrap();
    let mut guest = GuestDriver::new();
    let _session = activate(&block.handle, &guest);
    let (data, status, head) = request(&mut guest, VIRTIO_BLK_T_IN, 1, SECTOR_SIZE as u32);
    block.handle.queue_notifier.write(1).unwrap();
    tokio::time::timeout(WITHIN, irq.read())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(guest.used_idx(), 1);
    assert_eq!(
        guest.used_entry(0),
        (u32::from(head), SECTOR_SIZE as u32 + 1)
    );
    assert_eq!(guest.read_bytes(status, 1), [VIRTIO_BLK_S_OK]);
    assert_eq!(guest.read_bytes(data, SECTOR_SIZE), vec![0xcd; SECTOR_SIZE]);
    assert_eq!(block.handle.interrupt.status(), 1);
    block.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn write_request_reaches_only_the_correct_disk_sector() {
    let disk = TempDisk::new("write", &[0; 2 * SECTOR_SIZE]);
    let block = Block::new(&disk, false, Placement::VmmMain, Start::Running, None);
    let mut guest = GuestDriver::new();
    let _session = activate(&block.handle, &guest);
    let (data, status, _) = request(&mut guest, VIRTIO_BLK_T_OUT, 1, SECTOR_SIZE as u32);
    guest.write_bytes(data, &[0x5a; SECTOR_SIZE]);
    block.complete(&guest, 1).await;
    assert_eq!(guest.read_bytes(status, 1), [VIRTIO_BLK_S_OK]);
    assert_eq!(guest.used_entry(0).1, 1);
    assert_eq!(&disk.contents()[..SECTOR_SIZE], &[0; SECTOR_SIZE]);
    assert_eq!(&disk.contents()[SECTOR_SIZE..], &[0x5a; SECTOR_SIZE]);
    block.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn one_kick_drains_every_queued_request() {
    let disk = TempDisk::new("drain", &[0x11; 8 * SECTOR_SIZE]);
    let block = Block::new(&disk, false, Placement::VmmMain, Start::Running, None);
    let mut guest = GuestDriver::new();
    let _session = activate(&block.handle, &guest);
    let mut statuses = Vec::new();
    for sector in 0..5 {
        statuses.push(request(&mut guest, VIRTIO_BLK_T_IN, sector, SECTOR_SIZE as u32).1);
    }
    block.complete(&guest, 5).await;
    for status in statuses {
        assert_eq!(guest.read_bytes(status, 1), [VIRTIO_BLK_S_OK]);
    }
    block.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn rate_limit_spreads_requests_out_without_a_timerfd() {
    let disk = TempDisk::new("throttle", &[0x22; 8 * SECTOR_SIZE]);
    let block = Block::new(
        &disk,
        false,
        Placement::VmmMain,
        Start::Running,
        Some((1, Duration::from_millis(40))),
    );
    let irq =
        AsyncEventFd::from_event_fd(block.handle.interrupt.irq_fd().try_clone().unwrap()).unwrap();
    let mut guest = GuestDriver::new();
    let _session = activate(&block.handle, &guest);
    for sector in 0..3 {
        request(&mut guest, VIRTIO_BLK_T_IN, sector, SECTOR_SIZE as u32);
    }
    block.handle.queue_notifier.write(1).unwrap();
    tokio::time::timeout(WITHIN, irq.read())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(guest.used_idx(), 1);
    wait_until("rate limited requests", || guest.used_idx() == 3).await;
    block.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn flush_fragmented_id_unknown_type_and_disk_bounds() {
    let disk = TempDisk::new("misc", &[0; 2 * SECTOR_SIZE]);
    let block = Block::new(&disk, false, Placement::VmmMain, Start::Running, None);
    let mut guest = GuestDriver::new();
    let _session = activate(&block.handle, &guest);
    let flush_header = guest.request_header(VIRTIO_BLK_T_FLUSH, 0);
    let flush_status = guest.alloc(1);
    guest.submit(&[
        Segment::readable(flush_header, 16),
        Segment::writable(flush_status, 1),
    ]);
    let id_header = guest.request_header(VIRTIO_BLK_T_GET_ID, 0);
    let first = guest.alloc(3);
    let rest = guest.alloc(30);
    guest.write_bytes(rest, &[0xff; 30]);
    let id_status = guest.alloc(1);
    guest.submit(&[
        Segment::readable(id_header, 16),
        Segment::writable(first, 3),
        Segment::writable(rest, 30),
        Segment::writable(id_status, 1),
    ]);
    let bad_header = guest.request_header(0xdead, 0);
    let bad_status = guest.alloc(1);
    guest.submit(&[
        Segment::readable(bad_header, 16),
        Segment::writable(bad_status, 1),
    ]);
    let (_, oob_status, _) = request(&mut guest, VIRTIO_BLK_T_IN, u64::MAX, SECTOR_SIZE as u32);
    block.complete(&guest, 4).await;
    assert_eq!(guest.read_bytes(flush_status, 1), [VIRTIO_BLK_S_OK]);
    assert_eq!(guest.read_bytes(id_status, 1), [VIRTIO_BLK_S_OK]);
    assert_eq!(guest.read_bytes(first, 3), b"my-");
    assert_eq!(guest.read_bytes(rest, 4), b"disk");
    assert_eq!(guest.read_bytes(GuestAddress(rest.0 + 17), 13), [0xff; 13]);
    assert_eq!(guest.used_entry(1).1, 21);
    assert_eq!(guest.read_bytes(bad_status, 1), [VIRTIO_BLK_S_UNSUPP]);
    assert_eq!(guest.read_bytes(oob_status, 1), [VIRTIO_BLK_S_IOERR]);
    block.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn a_device_the_guest_never_activated_quiesces_and_stops() {
    let disk = TempDisk::new("inactive", &[0; SECTOR_SIZE]);
    let block = Block::new(&disk, false, Placement::Io, Start::Running, None);
    assert_eq!(
        block.control.quiesce(WITHIN).await,
        Response::Replied(BlockQuiesced::Inactive)
    );
    assert_eq!(block.runtime.running(), 1);
    block.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn read_only_disk_refuses_writes_and_advertises_capacity() {
    let disk = TempDisk::new("readonly", &[0x33; 2 * SECTOR_SIZE]);
    let block = Block::new(&disk, true, Placement::VmmMain, Start::Running, None);
    assert_ne!(block.handle.desc.features & (1 << 5), 0);
    assert_eq!(block.handle.desc.config, 2u64.to_le_bytes());
    assert_eq!(block.handle.desc.queue_max, [256]);
    let mut guest = GuestDriver::new();
    let _session = activate(&block.handle, &guest);
    let (data, status, _) = request(&mut guest, VIRTIO_BLK_T_OUT, 0, SECTOR_SIZE as u32);
    guest.write_bytes(data, &[0xff; SECTOR_SIZE]);
    block.complete(&guest, 1).await;
    assert_eq!(guest.read_bytes(status, 1), [VIRTIO_BLK_S_IOERR]);
    assert_eq!(disk.contents(), [0x33; 2 * SECTOR_SIZE]);
    block.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn wrong_descriptor_directions_are_rejected_before_disk_mutation() {
    let disk = TempDisk::new("directions", &[0x33; SECTOR_SIZE]);
    let block = Block::new(&disk, false, Placement::VmmMain, Start::Running, None);
    let mut guest = GuestDriver::new();
    let _session = activate(&block.handle, &guest);
    let mut statuses = Vec::new();
    for (kind, bad_header, writable) in [
        (VIRTIO_BLK_T_IN, false, false),
        (VIRTIO_BLK_T_OUT, false, true),
        (VIRTIO_BLK_T_OUT, true, false),
    ] {
        let header = guest.request_header(kind, 0);
        let data = guest.alloc(SECTOR_SIZE as u32);
        let status = guest.alloc(1);
        guest.write_bytes(data, &[0xff; SECTOR_SIZE]);
        guest.submit(&[
            Segment {
                addr: header,
                len: 16,
                device_writable: bad_header,
            },
            Segment {
                addr: data,
                len: SECTOR_SIZE as u32,
                device_writable: writable,
            },
            Segment::writable(status, 1),
        ]);
        statuses.push(status);
    }
    block.complete(&guest, 3).await;
    for status in statuses {
        assert_eq!(guest.read_bytes(status, 1), [VIRTIO_BLK_S_IOERR]);
    }
    assert_eq!(disk.contents(), [0x33; SECTOR_SIZE]);
    block.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn io_thread_transport_reset_acknowledges_and_reactivates_queues() {
    let disk = TempDisk::new("reset", &[0xac; SECTOR_SIZE]);
    let block = Block::new(&disk, true, Placement::Io, Start::Running, None);
    let mut guest = GuestDriver::new();
    let transport = block.transport(&guest);
    for _ in 0..3 {
        configure(&transport, USED_RING as u32, u32::from(QUEUE_SIZE));
        let (data, status, _) = request(&mut guest, VIRTIO_BLK_T_IN, 0, SECTOR_SIZE as u32);
        transport.write(0x50, &0u32.to_le_bytes());
        wait_until("transport completion", || guest.used_idx() == 1).await;
        assert_eq!(guest.read_bytes(status, 1), [VIRTIO_BLK_S_OK]);
        assert_eq!(guest.read_bytes(data, SECTOR_SIZE), [0xac; SECTOR_SIZE]);
        reset(transport.clone()).await;
        assert_eq!(reg(&transport, 0x70), 0);
        assert_eq!(reg(&transport, 0x60), 0);
        guest.reset_queue();
    }
    block.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn aggregate_write_past_disk_end_does_not_mutate_or_extend_disk() {
    let contents = [0x33; 2 * SECTOR_SIZE];
    let disk = TempDisk::new("aggregate-bounds", &contents);
    let original_len = std::fs::metadata(disk.path()).unwrap().len();
    let block = Block::new(&disk, false, Placement::VmmMain, Start::Running, None);
    let mut guest = GuestDriver::new();
    let _session = activate(&block.handle, &guest);
    let header = guest.request_header(VIRTIO_BLK_T_OUT, 1);
    let first = guest.alloc(SECTOR_SIZE as u32);
    let second = guest.alloc(SECTOR_SIZE as u32);
    let status = guest.alloc(1);
    guest.write_bytes(first, &[0x77; SECTOR_SIZE]);
    guest.write_bytes(second, &[0x88; SECTOR_SIZE]);
    guest.write_bytes(status, &[0xff]);
    guest.submit(&[
        Segment::readable(header, 16),
        Segment::readable(first, SECTOR_SIZE as u32),
        Segment::readable(second, SECTOR_SIZE as u32),
        Segment::writable(status, 1),
    ]);
    block.complete(&guest, 1).await;
    assert_eq!(guest.read_bytes(status, 1), [VIRTIO_BLK_S_IOERR]);
    assert_eq!(guest.used_entry(0).1, 1);
    assert_eq!(std::fs::metadata(disk.path()).unwrap().len(), original_len);
    assert_eq!(disk.contents(), contents);
    block.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn malformed_payload_does_not_partially_modify_the_disk() {
    let disk = TempDisk::new("invalid-payload", &[0x33; 2 * SECTOR_SIZE]);
    let block = Block::new(&disk, false, Placement::VmmMain, Start::Running, None);
    let mut guest = GuestDriver::new();
    let _session = activate(&block.handle, &guest);
    let header = guest.request_header(VIRTIO_BLK_T_OUT, 0);
    let data = guest.alloc(SECTOR_SIZE as u32);
    let status = guest.alloc(1);
    guest.write_bytes(data, &[0x77; SECTOR_SIZE]);
    guest.submit(&[
        Segment::readable(header, 16),
        Segment::readable(data, SECTOR_SIZE as u32),
        Segment::readable(GuestAddress(u64::MAX - 128), u32::MAX - 1024),
        Segment::writable(status, 1),
    ]);
    block.complete(&guest, 1).await;
    assert_eq!(guest.read_bytes(status, 1), [VIRTIO_BLK_S_IOERR]);
    assert_eq!(disk.contents(), [0x33; 2 * SECTOR_SIZE]);
    block.shutdown().await;
}

#[test]
fn mapped_zero_available_ring_is_a_local_failure_and_control_reports_exited() {
    const CHILD: &str = "DBE_BLOCK_ZERO_AVAIL_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new("timeout")
            .args(["--kill-after=1s", "10s"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "mapped_zero_available_ring_is_a_local_failure_and_control_reports_exited",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated worker failed ({}):\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        return;
    }

    tokio::runtime::LocalRuntime::new()
        .unwrap()
        .block_on(async {
            let (resources, handle) =
                BlockResources::open("zero-avail", &std::env::current_exe().unwrap(), true, None)
                    .unwrap();
            let (mut runtime, mut exits) = DeviceRuntime::new();
            let control = runtime
                .spawn_on(
                    "block",
                    Placement::VmmMain,
                    Start::Running,
                    move |control| async move { resources.into_device()?.run(control).await },
                )
                .unwrap();
            let guest = GuestDriver::new();
            let mut queue = guest.queue();
            queue.set_avail_ring_address(Some(0), Some(0));
            guest.mem.write_obj(1u16.to_le(), GuestAddress(2)).unwrap();
            assert!(queue.is_valid(guest.mem.as_ref()));
            let (mut session_control, session) = session_channel();
            handle
                .activator
                .activate(Activation {
                    mem: guest.mem.clone(),
                    queues: vec![queue],
                    interrupt: handle.interrupt.clone(),
                    driver_features: 1 << 32,
                    session,
                })
                .unwrap();
            let exit = tokio::time::timeout(WITHIN, exits.next())
                .await
                .unwrap()
                .unwrap();
            let error = exit.result.unwrap_err();
            assert!(matches!(
                error.source().unwrap().downcast_ref::<BlockError>(),
                Some(BlockError::Queue(virtio_queue::Error::QueueNotReady))
            ));
            assert_eq!(guest.used_idx(), 0);
            assert_eq!(control.quiesce(WITHIN).await, Response::Exited);
            assert_eq!(control.resume(WITHIN).await, Response::Exited);
            session_control.reset(Duration::ZERO).unwrap();
            runtime.shutdown(WITHIN).await.unwrap();
        });
}

#[tokio::test(flavor = "local")]
async fn corrupt_available_index_is_a_local_failure_and_control_reports_exited() {
    let disk = TempDisk::new("invalid-avail", &[0; SECTOR_SIZE]);
    let mut block = Block::new(&disk, true, Placement::Io, Start::Running, None);
    let guest = GuestDriver::new();
    guest
        .mem
        .write_obj(17u16, GuestAddress(AVAIL_RING + 2))
        .unwrap();
    let session = activate(&block.handle, &guest);
    let exit = tokio::time::timeout(WITHIN, block.exits.next())
        .await
        .unwrap()
        .unwrap();
    let error = exit.result.unwrap_err();
    assert!(
        error
            .source()
            .unwrap()
            .to_string()
            .contains("available index")
    );
    assert_eq!(block.control.quiesce(WITHIN).await, Response::Exited);
    assert_eq!(block.control.resume(WITHIN).await, Response::Exited);
    reset_session(session).await.unwrap();
    block.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn two_mib_reads_and_writes_span_bounded_staging_chunks() {
    const SIZE: usize = 2 * 1024 * 1024;
    let disk = TempDisk::new("large", &vec![0; SIZE]);
    let block = Block::new(&disk, false, Placement::Io, Start::Running, None);
    let mut guest = GuestDriver::new();
    let session = activate(&block.handle, &guest);
    let header = guest.request_header(VIRTIO_BLK_T_OUT, 0);
    let data = guest.alloc(SIZE as u32);
    let status = guest.alloc(1);
    guest.write_bytes(data, &vec![0xa5; SIZE]);
    guest.submit(&[
        Segment::readable(header, 16),
        Segment::readable(data, SIZE as u32),
        Segment::writable(status, 1),
    ]);
    block.complete(&guest, 1).await;
    assert_eq!(guest.read_bytes(status, 1), [VIRTIO_BLK_S_OK]);
    assert_eq!(disk.contents(), vec![0xa5; SIZE]);
    reset_session(session).await.unwrap();
    guest.reset_queue();
    let _session = activate(&block.handle, &guest);
    let (data, status, _) = request(&mut guest, VIRTIO_BLK_T_IN, 0, SIZE as u32);
    block.complete(&guest, 1).await;
    assert_eq!(guest.read_bytes(status, 1), [VIRTIO_BLK_S_OK]);
    assert_eq!(guest.read_bytes(data, SIZE), vec![0xa5; SIZE]);
    block.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn queue_alignment_geometry_and_bounds_fail_before_activation() {
    let disk = TempDisk::new("geometry", &[0; SECTOR_SIZE]);
    let (resources, handle) = BlockResources::open("geometry", disk.path(), true, None).unwrap();
    let guest = GuestDriver::new();
    let transport = MmioTransport::new(
        handle.desc,
        guest.mem,
        handle.activator,
        handle.interrupt,
        vec![handle.queue_notifier],
    );
    for (used, size) in [
        (1, 16),
        (USED_RING as u32, 0),
        (USED_RING as u32, 17),
        (USED_RING as u32, 512),
        (0x400000, 16),
    ] {
        configure(&transport, used, size);
        assert_ne!(reg(&transport, 0x70) & NEEDS_RESET, 0);
        assert_ne!(reg(&transport, 0x60) & 2, 0);
        transport.write(0x70, &0u32.to_le_bytes());
        assert_eq!(reg(&transport, 0x70), 0);
    }
    drop(resources);
}

#[tokio::test(flavor = "local")]
async fn truncated_chain_does_not_write_an_unreachable_status() {
    let disk = TempDisk::new("truncated", &[0x33; SECTOR_SIZE]);
    let block = Block::new(&disk, false, Placement::VmmMain, Start::Running, None);
    let mut guest = GuestDriver::new();
    let _session = activate(&block.handle, &guest);
    let header = guest.request_header(VIRTIO_BLK_T_OUT, 0);
    let status = guest.alloc(1);
    guest.write_bytes(status, &[0xff]);
    guest.submit(&[
        Segment::readable(header, 16),
        Segment::readable(GuestAddress(0), u32::MAX),
        Segment::writable(status, 1),
    ]);
    block.complete(&guest, 1).await;
    assert_eq!(guest.used_entry(0).1, 0);
    assert_eq!(guest.read_bytes(status, 1), [0xff]);
    assert_eq!(disk.contents(), [0x33; SECTOR_SIZE]);
    block.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn quiesce_parks_the_same_session_and_exports_queue_progress() {
    let disk = TempDisk::new("quiesce", &[0xab; SECTOR_SIZE]);
    let block = Block::new(&disk, false, Placement::VmmMain, Start::Running, None);
    let mut guest = GuestDriver::new();
    let _session = activate(&block.handle, &guest);
    request(&mut guest, VIRTIO_BLK_T_IN, 0, SECTOR_SIZE as u32);
    block.complete(&guest, 1).await;
    let Response::Replied(BlockQuiesced::Active(state)) = block.control.quiesce(WITHIN).await
    else {
        panic!("not active");
    };
    assert_eq!((state.next_avail, state.next_used), (1, 1));
    let (data, status, _) = request(&mut guest, VIRTIO_BLK_T_IN, 0, SECTOR_SIZE as u32);
    block.handle.queue_notifier.write(1).unwrap();
    assert_eq!(
        block.control.quiesce(WITHIN).await,
        Response::Replied(BlockQuiesced::Active(state))
    );
    assert_eq!(guest.used_idx(), 1);
    assert_eq!(guest.read_bytes(status, 1), [0xff]);
    assert_eq!(block.control.resume(WITHIN).await, Response::Replied(()));
    wait_until("resumed request", || guest.used_idx() == 2).await;
    assert_eq!(guest.read_bytes(data, SECTOR_SIZE), [0xab; SECTOR_SIZE]);
    assert_eq!(guest.read_bytes(status, 1), [VIRTIO_BLK_S_OK]);
    block.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn activation_already_queued_when_quiesce_arrives_reports_active_without_io() {
    let disk = TempDisk::new("race", &[0xab; SECTOR_SIZE]);
    let block = Block::new(&disk, false, Placement::VmmMain, Start::Parked, None);
    let mut guest = GuestDriver::new();
    let _session = activate(&block.handle, &guest);
    request(&mut guest, VIRTIO_BLK_T_IN, 0, SECTOR_SIZE as u32);
    let Response::Replied(BlockQuiesced::Active(state)) = block.control.quiesce(WITHIN).await
    else {
        panic!("queued activation reported inactive");
    };
    assert_eq!((state.next_avail, state.next_used), (0, 0));
    assert_eq!(guest.used_idx(), 0);
    assert_eq!(block.control.resume(WITHIN).await, Response::Replied(()));
    wait_until("first resumed request", || guest.used_idx() == 1).await;
    block.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn reset_while_quiesced_releases_the_session_without_resuming_io() {
    let disk = TempDisk::new("parked-reset", &[0xab; SECTOR_SIZE]);
    let block = Block::new(&disk, false, Placement::Io, Start::Running, None);
    let mut guest = GuestDriver::new();
    let transport = block.transport(&guest);
    configure(&transport, USED_RING as u32, u32::from(QUEUE_SIZE));
    assert!(matches!(
        block.control.quiesce(WITHIN).await,
        Response::Replied(BlockQuiesced::Active(_))
    ));
    let (_, status, _) = request(&mut guest, VIRTIO_BLK_T_IN, 0, SECTOR_SIZE as u32);
    transport.write(0x50, &0u32.to_le_bytes());
    reset(transport.clone()).await;
    assert_eq!(reg(&transport, 0x70), 0);
    assert_eq!(guest.used_idx(), 0);
    assert_eq!(guest.read_bytes(status, 1), [0xff]);
    assert_eq!(
        block.control.quiesce(WITHIN).await,
        Response::Replied(BlockQuiesced::Inactive)
    );
    guest.reset_queue();
    configure(&transport, USED_RING as u32, u32::from(QUEUE_SIZE));
    let (_, status, _) = request(&mut guest, VIRTIO_BLK_T_IN, 0, SECTOR_SIZE as u32);
    transport.write(0x50, &0u32.to_le_bytes());
    assert!(matches!(
        block.control.quiesce(WITHIN).await,
        Response::Replied(BlockQuiesced::Active(_))
    ));
    assert_eq!(guest.used_idx(), 0);
    assert_eq!(block.control.resume(WITHIN).await, Response::Replied(()));
    wait_until("new session after parked reset", || guest.used_idx() == 1).await;
    assert_eq!(guest.read_bytes(status, 1), [VIRTIO_BLK_S_OK]);
    block.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn reset_acknowledgement_follows_release_of_old_queue_and_ram() {
    let disk = TempDisk::new("release", &[0; SECTOR_SIZE]);
    let block = Block::new(&disk, false, Placement::Io, Start::Running, None);
    let guest = GuestDriver::new();
    let mut session = activate(&block.handle, &guest);
    assert!(matches!(
        block.control.quiesce(WITHIN).await,
        Response::Replied(BlockQuiesced::Active(_))
    ));
    let memory = Arc::downgrade(&guest.mem);
    drop(guest);
    tokio::task::spawn_blocking(move || {
        session.reset(WITHIN).unwrap();
        assert!(
            memory.upgrade().is_none(),
            "reset acknowledged before releasing RAM"
        );
    })
    .await
    .unwrap();
    assert_eq!(
        block.control.quiesce(WITHIN).await,
        Response::Replied(BlockQuiesced::Inactive)
    );
    block.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn failed_reset_keeps_the_old_session_and_registers_until_acknowledged() {
    let disk = TempDisk::new("failed-reset", &[0; SECTOR_SIZE]);
    let (_resources, handle) =
        BlockResources::open("failed-reset", disk.path(), true, None).unwrap();
    let (activator, mut receiver) = activation_channel();
    let guest = GuestDriver::new();
    let transport = Arc::new(MmioTransport::new(
        handle.desc,
        guest.mem,
        activator,
        handle.interrupt,
        vec![handle.queue_notifier],
    ));
    configure(&transport, USED_RING as u32, u32::from(QUEUE_SIZE));
    let old = receiver.recv().await.unwrap();
    reset(transport.clone()).await;
    assert_eq!(reg(&transport, 0x70), 15 | NEEDS_RESET);
    assert_eq!(reg(&transport, 0x44), 1);
    assert_eq!(reg(&transport, 0x60), 2);
    configure(&transport, USED_RING as u32, u32::from(QUEUE_SIZE));
    assert_eq!(reg(&transport, 0x70), 15 | NEEDS_RESET);
    assert!(
        receiver.try_recv().is_none(),
        "failed reset admitted a second queue owner"
    );
    drop(old);
    reset(transport.clone()).await;
    assert_eq!(reg(&transport, 0x70), 0);
    configure(&transport, USED_RING as u32, u32::from(QUEUE_SIZE));
    assert!(receiver.try_recv().is_some());
}

#[tokio::test(flavor = "local")]
async fn activation_is_bounded_and_rejected_or_abandoned_sessions_release_ram() {
    let disk = TempDisk::new("bounded-activation", &[0; SECTOR_SIZE]);
    let (_resources, handle) = BlockResources::open("bounded", disk.path(), false, None).unwrap();
    let (activator, receiver) = activation_channel();
    let guest = GuestDriver::new();
    let memory = Arc::downgrade(&guest.mem);
    let (mut first, first_session) = session_channel();
    activator
        .activate(Activation {
            queues: vec![guest.queue()],
            mem: guest.mem.clone(),
            interrupt: handle.interrupt.clone(),
            driver_features: 1 << 32,
            session: first_session,
        })
        .unwrap();
    let (mut second, second_session) = session_channel();
    assert_eq!(
        activator.activate(Activation {
            queues: vec![guest.queue()],
            mem: guest.mem.clone(),
            interrupt: handle.interrupt.clone(),
            driver_features: 1 << 32,
            session: second_session
        }),
        Err(ActivationError::Full)
    );
    second.reset(Duration::ZERO).unwrap();
    assert_eq!(
        first.reset(Duration::from_millis(1)),
        Err(ResetError::TimedOut)
    );
    drop(guest);
    drop(receiver);
    first.reset(Duration::ZERO).unwrap();
    assert!(memory.upgrade().is_none());
}

#[tokio::test(flavor = "local")]
async fn a_panicking_factory_releases_queued_activation_and_reports_exited() {
    let disk = TempDisk::new("panic", &[0; SECTOR_SIZE]);
    let (resources, handle) = BlockResources::open("panic", disk.path(), false, None).unwrap();
    let (mut runtime, mut exits) = DeviceRuntime::new();
    let control: DeviceHandle<BlockQuiesced> = runtime
        .spawn_on(
            "panic-block",
            Placement::VmmMain,
            Start::Running,
            move |control| async move {
                let device = resources.into_device()?;
                assert_eq!(std::thread::current().name(), Some("not-the-device-thread"));
                device.run(control).await
            },
        )
        .unwrap();
    let guest = GuestDriver::new();
    let mut session = activate(&handle, &guest);
    let memory = Arc::downgrade(&guest.mem);
    drop(guest);
    let exit = tokio::time::timeout(WITHIN, exits.next())
        .await
        .unwrap()
        .unwrap();
    assert!(exit.result.unwrap_err().to_string().contains("panicked"));
    assert_eq!(control.quiesce(WITHIN).await, Response::Exited);
    assert_eq!(control.resume(WITHIN).await, Response::Exited);
    session.reset(Duration::ZERO).unwrap();
    assert!(memory.upgrade().is_none());
    runtime.shutdown(WITHIN).await.unwrap();
}

#[tokio::test(flavor = "local")]
async fn notification_batch_bound_yields_to_cooperative_control() {
    let disk = TempDisk::new("batch", &[0xab; SECTOR_SIZE]);
    let block = Block::new(&disk, false, Placement::VmmMain, Start::Parked, None);
    let mut guest = GuestDriver::with_queue_size(256);
    let _session = activate(&block.handle, &guest);
    for _ in 0..70 {
        request(&mut guest, VIRTIO_BLK_T_IN, 0, SECTOR_SIZE as u32);
    }
    assert_eq!(block.control.resume(WITHIN).await, Response::Replied(()));
    wait_until("first batch progress", || guest.used_idx() > 0).await;
    let Response::Replied(BlockQuiesced::Active(state)) = block.control.quiesce(WITHIN).await
    else {
        panic!("not active");
    };
    assert_eq!(state.next_avail, state.next_used);
    assert!((1..=70).contains(&state.next_used));
    assert_eq!(guest.used_idx(), state.next_used);
    assert_eq!(block.control.resume(WITHIN).await, Response::Replied(()));
    wait_until("remaining batch", || guest.used_idx() == 70).await;
    block.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn invalid_status_descriptor_and_non_sector_payload_do_not_mutate_disk() {
    let disk = TempDisk::new("invalid-status", &[0x33; SECTOR_SIZE]);
    let block = Block::new(&disk, false, Placement::VmmMain, Start::Running, None);
    let mut guest = GuestDriver::new();
    let _session = activate(&block.handle, &guest);
    let header = guest.request_header(VIRTIO_BLK_T_OUT, 0);
    let data = guest.alloc(SECTOR_SIZE as u32);
    let status = guest.alloc(1);
    guest.write_bytes(data, &[0x77; SECTOR_SIZE]);
    guest.write_bytes(status, &[0xff]);
    guest.submit(&[
        Segment::readable(header, 16),
        Segment::readable(data, SECTOR_SIZE as u32),
        Segment::readable(status, 1),
    ]);
    let (_, odd_status, _) = request(&mut guest, VIRTIO_BLK_T_OUT, 0, 513);
    block.complete(&guest, 2).await;
    assert_eq!(guest.used_entry(0).1, 0);
    assert_eq!(guest.read_bytes(status, 1), [0xff]);
    assert_eq!(guest.read_bytes(odd_status, 1), [VIRTIO_BLK_S_IOERR]);
    assert_eq!(disk.contents(), [0x33; SECTOR_SIZE]);
    block.shutdown().await;
}
