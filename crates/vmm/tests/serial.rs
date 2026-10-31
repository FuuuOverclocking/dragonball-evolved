//! The serial console's input task: back-pressure, end of file, and the
//! control protocol, with the test acting as both host and guest.

use std::io::{self, PipeWriter, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time;
use vmm::bus::BusDevice;
use vmm::devices::async_fd::AsyncEventFd;
use vmm::devices::legacy::serial::{FIFO_SIZE, SerialDevice, SerialPort};
use vmm::devices::runtime::{DeviceExits, DeviceHandle, DeviceRuntime, Response, Start};

const TIMEOUT: Duration = Duration::from_secs(5);
const DATA: u16 = 0;
const IER: u16 = 1;
const LSR: u16 = 5;
const LSR_DATA_READY: u8 = 1;
const IER_RX_AVAILABLE: u8 = 1;

/// Polls `condition` while letting the device task run, until it holds.
async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..2000 {
        if condition() {
            return;
        }
        time::sleep(Duration::from_millis(1)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Lets other tasks run without waiting for anything in particular.
async fn settle() {
    for _ in 0..20 {
        time::sleep(Duration::from_millis(1)).await;
    }
}

/// Reads the receive FIFO empty the way a guest driver does. Emptying it is
/// what wakes a back-pressured device.
fn drain<W: Write + Send>(port: &SerialPort<W>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut byte = [0];
    loop {
        port.read(LSR, &mut byte);
        if byte[0] & LSR_DATA_READY == 0 {
            return out;
        }
        port.read(DATA, &mut byte);
        out.push(byte[0]);
    }
}

struct Console {
    runtime: DeviceRuntime,
    exits: DeviceExits,
    handle: DeviceHandle<()>,
    port: SerialPort<io::Sink>,
    host: PipeWriter,
}

fn console(start: Start) -> Console {
    let (host_rx, host) = io::pipe().unwrap();
    let (device, port) = SerialDevice::new(Some(host_rx.into()), io::sink()).unwrap();
    let (mut runtime, exits) = DeviceRuntime::new();
    let handle = runtime
        .spawn("serial", start, move |control| device.run(control))
        .unwrap();
    Console {
        runtime,
        exits,
        handle,
        port,
        host,
    }
}

impl Console {
    async fn shutdown(self) {
        time::timeout(TIMEOUT, self.runtime.shutdown(TIMEOUT))
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test(flavor = "local")]
async fn input_stops_at_the_fifo_boundary_and_resumes_once_the_guest_reads() {
    let mut console = console(Start::Running);
    console.host.write_all(&[b'a'; FIFO_SIZE]).unwrap();
    console.host.write_all(&[b'b'; 10]).unwrap();

    let port = &console.port;
    wait_until("the fifo to fill", || port.queued_input() == FIFO_SIZE).await;
    settle().await;
    assert_eq!(port.queued_input(), FIFO_SIZE, "device overran the fifo");

    assert_eq!(drain(port), vec![b'a'; FIFO_SIZE]);
    wait_until("the remainder to arrive", || port.queued_input() == 10).await;
    assert_eq!(drain(port), vec![b'b'; 10]);
    console.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn host_input_reaches_the_guest_fifo_and_raises_the_interrupt() {
    let mut console = console(Start::Running);
    let irq = AsyncEventFd::from_event_fd(console.port.irq_fd().try_clone().unwrap()).unwrap();
    console.port.write(IER, &[IER_RX_AVAILABLE]);
    console.host.write_all(b"hello").unwrap();

    let port = &console.port;
    wait_until("input to arrive", || port.queued_input() == 5).await;
    assert!(time::timeout(TIMEOUT, irq.read()).await.unwrap().unwrap() > 0);
    assert_eq!(drain(port), b"hello");
    console.shutdown().await;
}

/// Guest output, as the host would see it.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "local")]
async fn guest_output_reaches_the_host_without_input() {
    let output = Captured::default();
    let (device, port) = SerialDevice::new(None, output.clone()).unwrap();
    let (mut runtime, _exits) = DeviceRuntime::new();
    let handle = runtime
        .spawn("serial", Start::Running, move |control| device.run(control))
        .unwrap();
    for byte in b"ok" {
        port.write(DATA, &[*byte]);
    }
    assert_eq!(output.0.lock().unwrap().as_slice(), b"ok");
    // Without host input the task only answers its control.
    assert_eq!(handle.quiesce(TIMEOUT).await, Response::Replied(()));
    assert_eq!(handle.resume(TIMEOUT).await, Response::Replied(()));
    runtime.shutdown(TIMEOUT).await.unwrap();
}

#[tokio::test(flavor = "local")]
async fn closing_the_host_side_ends_the_device() {
    let mut console = console(Start::Running);
    settle().await;
    assert_eq!(console.runtime.running(), 1);

    drop(console.host);
    let exit = time::timeout(TIMEOUT, console.exits.next())
        .await
        .unwrap()
        .expect("the device reports its exit");
    assert_eq!(&*exit.name, "serial");
    exit.result.expect("closing the console is not a failure");
    assert_eq!(console.runtime.running(), 0);
    assert_eq!(console.handle.quiesce(TIMEOUT).await, Response::Exited);
}

#[tokio::test(flavor = "local")]
async fn a_back_pressured_device_still_answers_stop() {
    let mut console = console(Start::Running);
    console.host.write_all(&[b'x'; FIFO_SIZE + 10]).unwrap();
    let port = &console.port;
    wait_until("the fifo to fill", || port.queued_input() == FIFO_SIZE).await;
    console.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn a_quiesced_device_leaves_the_fifo_alone_until_resumed() {
    let mut console = console(Start::Running);
    console.host.write_all(b"before").unwrap();
    let port = &console.port;
    wait_until("initial data", || port.queued_input() == 6).await;
    assert_eq!(drain(port), b"before");

    assert_eq!(console.handle.quiesce(TIMEOUT).await, Response::Replied(()));
    console.host.write_all(b"during").unwrap();
    settle().await;
    assert_eq!(
        port.queued_input(),
        0,
        "data entered the fifo while quiesced"
    );

    assert_eq!(console.handle.resume(TIMEOUT).await, Response::Replied(()));
    wait_until("resumed data to arrive", || port.queued_input() == 6).await;
    assert_eq!(drain(port), b"during");
    console.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn a_quiesced_back_pressured_device_ignores_the_guest_draining() {
    let mut console = console(Start::Running);
    console.host.write_all(&[b'a'; FIFO_SIZE]).unwrap();
    console.host.write_all(b"rest").unwrap();
    let port = &console.port;
    wait_until("the fifo to fill", || port.queued_input() == FIFO_SIZE).await;

    assert_eq!(console.handle.quiesce(TIMEOUT).await, Response::Replied(()));
    assert_eq!(drain(port), vec![b'a'; FIFO_SIZE]);
    settle().await;
    assert_eq!(
        port.queued_input(),
        0,
        "data entered the fifo while quiesced"
    );

    assert_eq!(console.handle.resume(TIMEOUT).await, Response::Replied(()));
    wait_until("the rest to arrive", || port.queued_input() == 4).await;
    assert_eq!(drain(port), b"rest");
    console.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn a_device_spawned_parked_reads_nothing_until_resumed() {
    let mut console = console(Start::Parked);
    console.host.write_all(b"early").unwrap();
    settle().await;
    assert_eq!(console.port.queued_input(), 0, "a parked device read input");

    assert_eq!(console.handle.resume(TIMEOUT).await, Response::Replied(()));
    let port = &console.port;
    wait_until("input after resume", || port.queued_input() == 5).await;
    assert_eq!(drain(port), b"early");
    console.shutdown().await;
}
