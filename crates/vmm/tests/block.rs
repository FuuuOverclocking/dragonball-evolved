//! virtio-block, driven the way a guest would drive it.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::guest::{
    GuestDriver, SECTOR_SIZE, Segment, VIRTIO_BLK_S_IOERR, VIRTIO_BLK_S_OK, VIRTIO_BLK_S_UNSUPP,
    VIRTIO_BLK_T_FLUSH, VIRTIO_BLK_T_GET_ID, VIRTIO_BLK_T_IN, VIRTIO_BLK_T_OUT,
};
use common::{TempDisk, settle, wait_until};
use vmm::devices::DeviceManager;
use vmm::devices::async_fd::AsyncEventFd;
use vmm::devices::virtio::Activation;
use vmm::devices::virtio::block::{BlockDevice, BlockHandle};

/// Spawn a block device and bring it up the way a driver would.
///
/// Activation deliberately runs on another thread: in a real vmm the guest's
/// `DRIVER_OK` write is a synchronous mmio exit handled on a vcpu thread, so the
/// handshake has to cross a thread boundary. It is a channel send here rather
/// than firecracker's `activate_evt` eventfd round-trip.
fn activate(handle: &BlockHandle, guest: &GuestDriver) {
    let activation = Activation {
        mem: Arc::clone(&guest.mem),
        queues: vec![guest.queue()],
        interrupt: Arc::clone(&handle.interrupt),
    };
    let activator = handle.activator.clone();
    std::thread::spawn(move || activator.activate(activation).expect("activate device"))
        .join()
        .expect("activation thread panicked");
}

#[tokio::test(flavor = "local")]
async fn read_request_moves_disk_contents_into_guest_memory() {
    // Sector 0 and sector 1 hold different bytes, so reading the wrong offset is
    // an assertion failure rather than a coincidence.
    let mut contents = vec![0xab; SECTOR_SIZE];
    contents.extend_from_slice(&[0xcd; SECTOR_SIZE]);
    let disk = TempDisk::new("read", &contents);

    let (device, handle) = BlockDevice::new("poc-blk", disk.path(), false, None).unwrap();
    let irq = AsyncEventFd::from_event_fd(handle.interrupt.irq_fd().try_clone().unwrap()).unwrap();

    let mut devices = DeviceManager::new();
    let stop = devices.stop_signal();
    devices.spawn("block", device.run(stop));

    let mut guest = GuestDriver::new();
    activate(&handle, &guest);

    let header = guest.request_header(VIRTIO_BLK_T_IN, 1);
    let data = guest.alloc(SECTOR_SIZE as u32);
    let status = guest.alloc(1);
    let head = guest.submit(&[
        Segment::readable(header, 16),
        Segment::writable(data, SECTOR_SIZE as u32),
        Segment::writable(status, 1),
    ]);

    // What KVM does when the guest writes the queue notification register.
    handle.queue_notifier.write(1).unwrap();

    // The device raises its used-ring interrupt when it is done.
    irq.read().await.unwrap();

    assert_eq!(guest.used_idx(), 1);
    let (used_id, used_len) = guest.used_entry(0);
    assert_eq!(used_id as u16, head);
    // Payload plus the status byte.
    assert_eq!(used_len, SECTOR_SIZE as u32 + 1);
    assert_eq!(guest.read_bytes(status, 1), [VIRTIO_BLK_S_OK]);
    assert_eq!(guest.read_bytes(data, SECTOR_SIZE), vec![0xcd; SECTOR_SIZE]);

    devices.quiesce().await;
}

#[tokio::test(flavor = "local")]
async fn write_request_reaches_the_disk() {
    let disk = TempDisk::new("write", &vec![0; 2 * SECTOR_SIZE]);

    let (device, handle) = BlockDevice::new("poc-blk", disk.path(), false, None).unwrap();
    let irq = AsyncEventFd::from_event_fd(handle.interrupt.irq_fd().try_clone().unwrap()).unwrap();

    let mut devices = DeviceManager::new();
    let stop = devices.stop_signal();
    devices.spawn("block", device.run(stop));

    let mut guest = GuestDriver::new();
    activate(&handle, &guest);

    let header = guest.request_header(VIRTIO_BLK_T_OUT, 1);
    let data = guest.alloc(SECTOR_SIZE as u32);
    guest.write_bytes(data, &[0x5a; SECTOR_SIZE]);
    let status = guest.alloc(1);
    guest.submit(&[
        Segment::readable(header, 16),
        Segment::readable(data, SECTOR_SIZE as u32),
        Segment::writable(status, 1),
    ]);

    handle.queue_notifier.write(1).unwrap();
    irq.read().await.unwrap();

    assert_eq!(guest.read_bytes(status, 1), [VIRTIO_BLK_S_OK]);
    // A write reports only the status byte as used.
    assert_eq!(guest.used_entry(0).1, 1);

    let on_disk = disk.contents();
    assert_eq!(&on_disk[..SECTOR_SIZE], &[0; SECTOR_SIZE]);
    assert_eq!(&on_disk[SECTOR_SIZE..], &[0x5a; SECTOR_SIZE]);

    devices.quiesce().await;
}

#[tokio::test(flavor = "local")]
async fn one_kick_drains_every_queued_request() {
    // The kernel coalesces eventfd writes into a counter, so a device must never
    // assume one notification means one request.
    let disk = TempDisk::new("drain", &vec![0x11; 8 * SECTOR_SIZE]);

    let (device, handle) = BlockDevice::new("poc-blk", disk.path(), false, None).unwrap();
    let irq = AsyncEventFd::from_event_fd(handle.interrupt.irq_fd().try_clone().unwrap()).unwrap();

    let mut devices = DeviceManager::new();
    let stop = devices.stop_signal();
    devices.spawn("block", device.run(stop));

    let mut guest = GuestDriver::new();
    activate(&handle, &guest);

    const REQUESTS: u16 = 5;
    let mut statuses = Vec::new();
    for sector in 0..u64::from(REQUESTS) {
        let header = guest.request_header(VIRTIO_BLK_T_IN, sector);
        let data = guest.alloc(SECTOR_SIZE as u32);
        let status = guest.alloc(1);
        guest.submit(&[
            Segment::readable(header, 16),
            Segment::writable(data, SECTOR_SIZE as u32),
            Segment::writable(status, 1),
        ]);
        statuses.push(status);
    }

    handle.queue_notifier.write(1).unwrap();
    irq.read().await.unwrap();

    wait_until("all requests to complete", || guest.used_idx() == REQUESTS).await;
    for status in statuses {
        assert_eq!(guest.read_bytes(status, 1), [VIRTIO_BLK_S_OK]);
    }

    devices.quiesce().await;
}

#[tokio::test(flavor = "local")]
async fn rate_limit_spreads_requests_out_without_a_timerfd() {
    let disk = TempDisk::new("throttle", &vec![0x22; 8 * SECTOR_SIZE]);

    // One request per window, so each extra request has to wait for a refill.
    let window = Duration::from_millis(40);
    let (device, handle) =
        BlockDevice::new("poc-blk", disk.path(), false, Some((1, window))).unwrap();
    let irq = AsyncEventFd::from_event_fd(handle.interrupt.irq_fd().try_clone().unwrap()).unwrap();

    let mut devices = DeviceManager::new();
    let stop = devices.stop_signal();
    devices.spawn("block", device.run(stop));

    let mut guest = GuestDriver::new();
    activate(&handle, &guest);

    const REQUESTS: u16 = 3;
    for sector in 0..u64::from(REQUESTS) {
        let header = guest.request_header(VIRTIO_BLK_T_IN, sector);
        let data = guest.alloc(SECTOR_SIZE as u32);
        let status = guest.alloc(1);
        guest.submit(&[
            Segment::readable(header, 16),
            Segment::writable(data, SECTOR_SIZE as u32),
            Segment::writable(status, 1),
        ]);
    }

    handle.queue_notifier.write(1).unwrap();
    irq.read().await.unwrap();

    // The budget only covered the first request; the rest are still held back.
    assert_eq!(
        guest.used_idx(),
        1,
        "rate limit did not stop the drain after the first request"
    );

    // Refills arrive from a `sleep` inside the device's own loop, not from a
    // timerfd registered in a shared epoll set.
    wait_until("the rate limit to let the rest through", || {
        guest.used_idx() == REQUESTS
    })
    .await;

    devices.quiesce().await;
}

#[tokio::test(flavor = "local")]
async fn flush_and_get_id_are_answered_and_junk_is_rejected() {
    let disk = TempDisk::new("misc", &vec![0; 2 * SECTOR_SIZE]);

    let (device, handle) = BlockDevice::new("my-disk", disk.path(), false, None).unwrap();
    let irq = AsyncEventFd::from_event_fd(handle.interrupt.irq_fd().try_clone().unwrap()).unwrap();

    let mut devices = DeviceManager::new();
    let stop = devices.stop_signal();
    devices.spawn("block", device.run(stop));

    let mut guest = GuestDriver::new();
    activate(&handle, &guest);

    let flush_header = guest.request_header(VIRTIO_BLK_T_FLUSH, 0);
    let flush_status = guest.alloc(1);
    guest.submit(&[
        Segment::readable(flush_header, 16),
        Segment::writable(flush_status, 1),
    ]);

    let id_header = guest.request_header(VIRTIO_BLK_T_GET_ID, 0);
    let id_data = guest.alloc(20);
    let id_status = guest.alloc(1);
    guest.submit(&[
        Segment::readable(id_header, 16),
        Segment::writable(id_data, 20),
        Segment::writable(id_status, 1),
    ]);

    // An unknown request type must be refused, not crash the device.
    let bad_header = guest.request_header(0xdead, 0);
    let bad_status = guest.alloc(1);
    guest.submit(&[
        Segment::readable(bad_header, 16),
        Segment::writable(bad_status, 1),
    ]);

    // Reading past the end of the disk must be refused too.
    let oob_header = guest.request_header(VIRTIO_BLK_T_IN, 1_000_000);
    let oob_data = guest.alloc(SECTOR_SIZE as u32);
    let oob_status = guest.alloc(1);
    guest.submit(&[
        Segment::readable(oob_header, 16),
        Segment::writable(oob_data, SECTOR_SIZE as u32),
        Segment::writable(oob_status, 1),
    ]);

    handle.queue_notifier.write(1).unwrap();
    irq.read().await.unwrap();
    wait_until("all four requests to complete", || guest.used_idx() == 4).await;

    assert_eq!(guest.read_bytes(flush_status, 1), [VIRTIO_BLK_S_OK]);
    assert_eq!(guest.read_bytes(id_status, 1), [VIRTIO_BLK_S_OK]);
    assert_eq!(&guest.read_bytes(id_data, 7), b"my-disk");
    assert_eq!(guest.read_bytes(bad_status, 1), [VIRTIO_BLK_S_UNSUPP]);
    assert_eq!(guest.read_bytes(oob_status, 1), [VIRTIO_BLK_S_IOERR]);

    devices.quiesce().await;
}

#[tokio::test(flavor = "local")]
async fn a_device_the_guest_never_activated_still_stops_cleanly() {
    // Firecracker needs an `activate_evt` in its epoll set to ever get control
    // back in this state. Here the task is simply parked on two awaits, one of
    // which is the stop signal.
    let disk = TempDisk::new("inactive", &vec![0; SECTOR_SIZE]);
    let (device, _handle) = BlockDevice::new("poc-blk", disk.path(), false, None).unwrap();

    let mut devices = DeviceManager::new();
    let stop = devices.stop_signal();
    devices.spawn("block", device.run(stop));

    settle().await;
    assert_eq!(devices.running(), 1);

    devices.quiesce().await;
    assert_eq!(devices.running(), 0);
}

#[tokio::test(flavor = "local")]
async fn read_only_disk_refuses_writes() {
    let disk = TempDisk::new("readonly", &vec![0x33; 2 * SECTOR_SIZE]);

    let (device, handle) = BlockDevice::new("poc-blk", disk.path(), true, None).unwrap();
    let irq = AsyncEventFd::from_event_fd(handle.interrupt.irq_fd().try_clone().unwrap()).unwrap();

    let mut devices = DeviceManager::new();
    let stop = devices.stop_signal();
    devices.spawn("block", device.run(stop));

    let mut guest = GuestDriver::new();
    activate(&handle, &guest);

    let header = guest.request_header(VIRTIO_BLK_T_OUT, 0);
    let data = guest.alloc(SECTOR_SIZE as u32);
    guest.write_bytes(data, &[0xff; SECTOR_SIZE]);
    let status = guest.alloc(1);
    guest.submit(&[
        Segment::readable(header, 16),
        Segment::readable(data, SECTOR_SIZE as u32),
        Segment::writable(status, 1),
    ]);

    handle.queue_notifier.write(1).unwrap();
    irq.read().await.unwrap();

    assert_eq!(guest.read_bytes(status, 1), [VIRTIO_BLK_S_IOERR]);
    assert_eq!(disk.contents(), vec![0x33; 2 * SECTOR_SIZE]);

    devices.quiesce().await;
}
