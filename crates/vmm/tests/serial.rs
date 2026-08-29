//! The serial console, and specifically its back-pressure.
//!
//! Firecracker implements the same three states by adding and removing the input
//! descriptor from its epoll set from inside `process()`. These tests exercise the
//! same transitions with nothing registered or unregistered along the way.

mod common;

use std::fs::File;
use std::io::{Write, sink};

use common::{pipe, settle, wait_until};
use vmm::devices::DeviceManager;
use vmm::devices::legacy::serial::{SerialDevice, SerialHandle};

/// vm-superio's receive FIFO holds this many bytes.
const FIFO_SIZE: usize = 64;
/// Offset of the UART's receive/transmit register.
const DATA_OFFSET: u8 = 0;

fn queued<W: Write + Send>(handle: &SerialHandle<W>) -> usize {
    FIFO_SIZE - handle.uart.lock().unwrap().fifo_capacity()
}

/// Act as the guest and read the whole receive FIFO.
///
/// Emptying it is what makes vm-superio report `in_buffer_empty`, which is the
/// signal the device task is parked on.
fn drain<W: Write + Send>(handle: &SerialHandle<W>) -> Vec<u8> {
    let mut uart = handle.uart.lock().unwrap();
    let mut out = Vec::new();
    while uart.fifo_capacity() < FIFO_SIZE {
        out.push(uart.read(DATA_OFFSET));
    }
    out
}

#[tokio::test(flavor = "local")]
async fn input_stops_at_the_fifo_boundary_and_resumes_once_the_guest_reads() {
    let (host_rx, host_tx) = pipe().unwrap();
    let (device, handle) = SerialDevice::new(host_rx, sink()).unwrap();

    let mut devices = DeviceManager::new();
    let stop = devices.stop_signal();
    devices.spawn("serial", device.run(stop));

    // More input than the FIFO can hold, so the device has to stop partway.
    let mut writer = File::from(host_tx);
    writer.write_all(&[b'a'; FIFO_SIZE]).unwrap();
    writer.write_all(&[b'b'; 10]).unwrap();

    wait_until("the fifo to fill", || queued(&handle) == FIFO_SIZE).await;

    // The device must now be idle rather than overrunning the FIFO or spinning.
    settle().await;
    assert_eq!(
        queued(&handle),
        FIFO_SIZE,
        "device overran the receive fifo"
    );

    assert_eq!(drain(&handle), vec![b'a'; FIFO_SIZE]);

    // Draining releases the back-pressure, with no descriptor re-registered.
    wait_until("the remainder to arrive", || queued(&handle) == 10).await;
    assert_eq!(drain(&handle), vec![b'b'; 10]);

    devices.quiesce().await;
}

#[tokio::test(flavor = "local")]
async fn host_input_reaches_the_guest_fifo() {
    let (host_rx, host_tx) = pipe().unwrap();
    let (device, handle) = SerialDevice::new(host_rx, sink()).unwrap();

    let mut devices = DeviceManager::new();
    let stop = devices.stop_signal();
    devices.spawn("serial", device.run(stop));

    let mut writer = File::from(host_tx);
    writer.write_all(b"hello").unwrap();

    wait_until("input to arrive", || queued(&handle) == 5).await;
    assert_eq!(drain(&handle), b"hello");

    devices.quiesce().await;
}

#[tokio::test(flavor = "local")]
async fn closing_the_host_side_ends_the_device() {
    let (host_rx, host_tx) = pipe().unwrap();
    let (device, _handle) = SerialDevice::new(host_rx, sink()).unwrap();

    let mut devices = DeviceManager::new();
    let stop = devices.stop_signal();
    devices.spawn("serial", device.run(stop));

    settle().await;
    assert_eq!(devices.running(), 1);

    // End of file. Firecracker unregisters both descriptors and leaves an inert
    // subscriber behind; here the task returns and takes its descriptor with it.
    drop(host_tx);

    let exit = devices
        .next_exit()
        .await
        .expect("device should report an exit");
    assert_eq!(exit.name, "serial");
    exit.result.expect("closing the console is not a failure");
    assert_eq!(devices.running(), 0);
}

#[tokio::test(flavor = "local")]
async fn a_blocked_device_still_answers_the_stop_signal() {
    let (host_rx, host_tx) = pipe().unwrap();
    let (device, handle) = SerialDevice::new(host_rx, sink()).unwrap();

    let mut devices = DeviceManager::new();
    let stop = devices.stop_signal();
    devices.spawn("serial", device.run(stop));

    // Wedge the device against a full FIFO that nothing will drain.
    let mut writer = File::from(host_tx);
    writer.write_all(&[b'x'; FIFO_SIZE + 10]).unwrap();
    wait_until("the fifo to fill", || queued(&handle) == FIFO_SIZE).await;

    // Shutting down while back-pressured must not hang: the stop signal is one
    // arm of the same `select!` the device is waiting on.
    devices.quiesce().await;
    assert_eq!(devices.running(), 0);
}
