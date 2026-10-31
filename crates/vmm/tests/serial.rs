//! The serial console's input task: back-pressure, end of file, and the
//! control protocol, with the test acting as both host and guest.

use std::io::{self, PipeWriter, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time;
use vmm::bus::BusDevice;
use vmm::devices::async_fd::AsyncEventFd;
use vmm::devices::legacy::serial::{
    FIFO_SIZE, SerialDevice, SerialError, SerialInputState, SerialPort, SerialSnapshot,
    SerialSnapshotError,
};
use vmm::devices::runtime::{DeviceExits, DeviceHandle, DeviceRuntime, Response, Start};

const TIMEOUT: Duration = Duration::from_secs(5);
const DATA: u16 = 0;
const IER: u16 = 1;
const LSR: u16 = 5;
const LSR_DATA_READY: u8 = 1;
const IER_RX_AVAILABLE: u8 = 1;

/// Observe actual FIFO progress, yielding to the device rather than sleeping.
async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    time::timeout(TIMEOUT, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// Wait for an acknowledged pending-byte boundary, leaving the task parked.
/// Repeated quiesces cannot race FIFO capture once the desired reply arrives.
async fn quiesce_with_pending(
    handle: &DeviceHandle<SerialInputState>,
    expected: &[u8],
) -> SerialInputState {
    time::timeout(TIMEOUT, async {
        loop {
            let Response::Replied(input) = handle.quiesce(TIMEOUT).await else {
                panic!("serial input did not quiesce");
            };
            if input.pending == expected {
                return input;
            }
            assert_eq!(handle.resume(TIMEOUT).await, Response::Replied(()));
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("serial did not reach the expected pending-byte boundary")
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
    handle: DeviceHandle<SerialInputState>,
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
    quiesce_with_pending(&console.handle, &[b'b'; 10]).await;
    assert_eq!(port.queued_input(), FIFO_SIZE, "device overran the fifo");

    assert_eq!(drain(port), vec![b'a'; FIFO_SIZE]);
    assert_eq!(console.handle.resume(TIMEOUT).await, Response::Replied(()));
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
    assert_eq!(
        handle.quiesce(TIMEOUT).await,
        Response::Replied(SerialInputState::default())
    );
    assert_eq!(handle.resume(TIMEOUT).await, Response::Replied(()));
    runtime.shutdown(TIMEOUT).await.unwrap();
}

#[tokio::test(flavor = "local")]
async fn closing_the_host_side_ends_the_device() {
    let mut console = console(Start::Running);
    assert_eq!(console.handle.resume(TIMEOUT).await, Response::Replied(()));
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

    let quiesced = quiesce_with_pending(&console.handle, &[]).await;
    console.host.write_all(b"during").unwrap();
    assert_eq!(
        console.handle.quiesce(TIMEOUT).await,
        Response::Replied(quiesced)
    );
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

    let quiesced = quiesce_with_pending(&console.handle, b"rest").await;
    assert_eq!(drain(port), vec![b'a'; FIFO_SIZE]);
    assert_eq!(
        console.handle.quiesce(TIMEOUT).await,
        Response::Replied(quiesced)
    );
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
    assert_eq!(
        console.handle.quiesce(TIMEOUT).await,
        Response::Replied(SerialInputState::default())
    );
    assert_eq!(console.port.queued_input(), 0, "a parked device read input");

    assert_eq!(console.handle.resume(TIMEOUT).await, Response::Replied(()));
    let port = &console.port;
    wait_until("input after resume", || port.queued_input() == 5).await;
    assert_eq!(drain(port), b"early");
    console.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn snapshot_restores_fifo_and_partial_host_chunk_even_without_new_host_input() {
    let mut console = console(Start::Running);
    // Also leave THRE pending: from_state would signal both enabled causes.
    console.port.write(IER, &[IER_RX_AVAILABLE | 2]);
    console.port.write(7, &[0x5a]);
    let prefix = vec![b'a'; FIFO_SIZE - 5];
    console.host.write_all(&prefix).unwrap();
    wait_until("initial FIFO prefix", || {
        console.port.queued_input() == prefix.len()
    })
    .await;
    let chunk: Vec<u8> = (0..SerialInputState::CAPACITY as u8).collect();
    console.host.write_all(&chunk).unwrap();
    let input = quiesce_with_pending(&console.handle, &chunk[5..]).await;
    let saved = SerialSnapshot {
        uart: console.port.capture_uart().unwrap(),
        input,
    };
    let mut fifo = prefix;
    fifo.extend_from_slice(&chunk[..5]);
    assert_eq!(saved.uart.in_buffer, fifo);
    assert_eq!(saved.uart.scratch, 0x5a);
    saved.validate().unwrap();
    console.shutdown().await;

    let output = Captured::default();
    let (device, port) = SerialDevice::restore(&saved, None, output.clone()).unwrap();
    assert_eq!(port.capture_uart().unwrap(), saved.uart);
    assert_eq!(
        port.irq_fd().read().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    let (mut runtime, _exits) = DeviceRuntime::new();
    let handle = runtime
        .spawn("restored-serial", Start::Parked, move |control| {
            device.run(control)
        })
        .unwrap();
    assert_eq!(
        handle.quiesce(TIMEOUT).await,
        Response::Replied(saved.input.clone())
    );
    // Stand in for the integrator's CPU/LAPIC/clock restore ordering boundary.
    port.reraise_pending().unwrap();
    assert_eq!(port.irq_fd().read().unwrap(), 1);
    assert_eq!(
        port.capture_uart().unwrap(),
        saved.uart,
        "reraise acknowledged the UART"
    );
    assert_eq!(drain(&port), fifo);
    assert_eq!(
        handle.quiesce(TIMEOUT).await,
        Response::Replied(saved.input.clone())
    );
    assert_eq!(
        port.queued_input(),
        0,
        "parked restored input entered the FIFO"
    );

    assert_eq!(handle.resume(TIMEOUT).await, Response::Replied(()));
    wait_until("saved pending bytes without host input", || {
        port.queued_input() == chunk.len() - 5
    })
    .await;
    assert_eq!(drain(&port), chunk[5..]);
    assert_eq!(
        handle.quiesce(TIMEOUT).await,
        Response::Replied(SerialInputState::default()),
        "no-host task did not drain pending input before becoming inert"
    );
    for byte in b"new output" {
        port.write(DATA, &[*byte]);
    }
    assert_eq!(output.0.lock().unwrap().as_slice(), b"new output");
    assert_eq!(handle.resume(TIMEOUT).await, Response::Replied(()));
    runtime.shutdown(TIMEOUT).await.unwrap();
}

#[tokio::test(flavor = "local")]
async fn restored_pending_bytes_precede_input_from_the_new_host_endpoint() {
    let saved = SerialSnapshot {
        input: SerialInputState {
            pending: b"saved ".to_vec(),
        },
        ..SerialSnapshot::default()
    };
    let (reader, mut host) = io::pipe().unwrap();
    host.write_all(b"new").unwrap();
    let (device, port) = SerialDevice::restore(&saved, Some(reader.into()), io::sink()).unwrap();
    let (mut runtime, _exits) = DeviceRuntime::new();
    let handle = runtime
        .spawn("restored-serial", Start::Parked, move |control| {
            device.run(control)
        })
        .unwrap();
    assert_eq!(
        handle.quiesce(TIMEOUT).await,
        Response::Replied(saved.input)
    );
    assert_eq!(port.queued_input(), 0);
    assert_eq!(handle.resume(TIMEOUT).await, Response::Replied(()));
    wait_until("saved then newly bound input", || port.queued_input() == 9).await;
    assert_eq!(drain(&port), b"saved new");
    runtime.shutdown(TIMEOUT).await.unwrap();
}

#[tokio::test(flavor = "local")]
async fn restored_loopback_keeps_pending_input_and_still_answers_control() {
    let mut saved = SerialSnapshot::default();
    saved.uart.modem_control |= 0x10;
    saved.input.pending = b"saved".to_vec();
    let (device, port) = SerialDevice::restore(&saved, None, io::sink()).unwrap();
    let (mut runtime, _exits) = DeviceRuntime::new();
    let handle = runtime
        .spawn("restored-serial", Start::Running, move |control| {
            device.run(control)
        })
        .unwrap();
    assert_eq!(
        handle.quiesce(TIMEOUT).await,
        Response::Replied(saved.input.clone())
    );
    assert_eq!(handle.resume(TIMEOUT).await, Response::Replied(()));
    assert_eq!(
        handle.quiesce(TIMEOUT).await,
        Response::Replied(saved.input.clone())
    );
    assert_eq!(port.queued_input(), 0);
    port.write(4, &[saved.uart.modem_control & !0x10]);
    assert_eq!(handle.resume(TIMEOUT).await, Response::Replied(()));
    wait_until("saved input after leaving loopback", || {
        port.queued_input() == 5
    })
    .await;
    assert_eq!(drain(&port), b"saved");
    runtime.shutdown(TIMEOUT).await.unwrap();
}

#[test]
fn restore_is_irq_silent_and_explicit_reraise_respects_pending_enabled_causes() {
    for (enabled, pending, expected) in [
        (1, 4, true),
        (2, 2, true),
        (3, 6, true),
        (0, 6, false),
        (3, 1, false),
    ] {
        let mut saved = SerialSnapshot::default();
        saved.uart.interrupt_enable = enabled;
        saved.uart.interrupt_identification = pending;
        let (_device, port) = SerialDevice::restore(&saved, None, io::sink()).unwrap();
        assert_eq!(
            port.irq_fd().read().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(port.capture_uart().unwrap(), saved.uart);
        port.reraise_pending().unwrap();
        if expected {
            assert_eq!(port.irq_fd().read().unwrap(), 1);
        } else {
            assert_eq!(
                port.irq_fd().read().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }
        assert_eq!(port.capture_uart().unwrap(), saved.uart);
    }
}

#[test]
fn invalid_fifo_or_pending_lengths_are_rejected_before_registering_host_input() {
    let mut fifo = SerialSnapshot::default();
    fifo.uart.in_buffer = vec![0; FIFO_SIZE + 1];
    let mut pending = SerialSnapshot::default();
    pending.input.pending = vec![0; SerialInputState::CAPACITY + 1];
    for saved in [fifo, pending] {
        let (reader, _host) = io::pipe().unwrap();
        let input: std::os::fd::OwnedFd = reader.into();
        let observer = input.try_clone().unwrap();
        let before = rustix::fs::fcntl_getfl(&observer).unwrap();
        // No reactor exists here. Reaching AsyncInput::new would be a bug.
        let error = SerialDevice::restore(&saved, Some(input), io::sink()).unwrap_err();
        assert!(matches!(
            error,
            SerialError::Snapshot(SerialSnapshotError::FifoLength(_))
                | SerialError::Snapshot(SerialSnapshotError::PendingLength(_))
        ));
        assert_eq!(rustix::fs::fcntl_getfl(&observer).unwrap(), before);
    }
}
