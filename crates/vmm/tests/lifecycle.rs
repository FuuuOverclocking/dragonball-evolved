//! The vmm thread's async main loop and its request dispatch.

mod common;

use std::fs::OpenOptions;
use std::time::Duration;

use api::{PocConfig, VmmRequest, VmmStatus};
use common::TempDisk;
use vmm::{VmmClient, VmmExitStatus};

const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

fn start(id: &str) -> VmmClient {
    vmm::start(id.into()).expect("start vmm")
}

fn shutdown(client: &VmmClient) -> VmmExitStatus {
    // The client runs on this thread while the loop runs on the vmm thread, so the
    // request crosses over through a channel that wakes the vmm task directly. The
    // reply is sent only after devices have quiesced, which makes it mean "shutdown
    // finished" rather than "shutdown noticed".
    //
    // `Shutdown` answers with `()`, so there is no response enum to match here and
    // no impossible variant to explain away.
    client
        .request_timeout(VmmRequest::Shutdown, REPLY_TIMEOUT)
        .expect("shutdown request should be answered");

    client.join_vmm_thread().expect("join vmm thread")
}

#[test]
fn the_vmm_thread_serves_a_shutdown_request_and_exits_cleanly() {
    let client = start("lifecycle-test");
    assert_eq!(client.instance_info().id, "lifecycle-test");

    let status = shutdown(&client);
    assert!(
        matches!(status, VmmExitStatus::Ok),
        "unexpected exit status: {status:?}"
    );
}

#[test]
fn a_query_answers_with_the_type_its_request_asked_for() {
    let client = start("status-test");

    // The response type comes from the request, so this annotation is checked
    // rather than asserted: `Status` cannot answer with anything but a `VmmStatus`.
    let status: VmmStatus = client
        .request_timeout(VmmRequest::Status, REPLY_TIMEOUT)
        .expect("status request should be answered");
    assert_eq!(status.devices_running, 0);
    assert!(!status.vcpu_running);
    assert_eq!(status.memory_bytes, 0);

    // A non-terminal request leaves the loop serving, so shutdown still works.
    assert!(matches!(shutdown(&client), VmmExitStatus::Ok));
}

#[test]
fn real_kvm_guest_drives_the_async_block_device() {
    if OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_err()
    {
        eprintln!("skipping KVM POC: /dev/kvm is unavailable");
        return;
    }

    let disk = TempDisk::new("kvm-poc", &[0x5a; 512]);
    let client = start("kvm-poc");
    let report = client
        .request_timeout(
            |reply| {
                VmmRequest::StartPoc(
                    PocConfig {
                        disk_path: disk.path().to_owned(),
                    },
                    reply,
                )
            },
            REPLY_TIMEOUT,
        )
        .expect("POC reply channel")
        .expect("run KVM POC");

    assert_eq!(report.used_index, 1);
    assert_eq!(report.request_status, 0);
    assert_eq!(report.data, vec![0x5a; 16]);
    assert_eq!(report.interrupt_status & 1, 1);
    assert!(matches!(
        client.join_vmm_thread().expect("join vmm thread"),
        VmmExitStatus::Ok
    ));
}

#[test]
fn several_vmms_run_side_by_side_in_one_process() {
    // Nothing about a vmm is process-global. Instance info used to live in a
    // `OnceLock`, which made the second `start` fail and forced every test that
    // needs a live vmm into a test binary of its own.
    let clients: Vec<_> = (0..3).map(|i| start(&format!("vmm-{i}"))).collect();

    for (i, client) in clients.iter().enumerate() {
        assert_eq!(client.instance_info().id, format!("vmm-{i}"));
    }
    for client in &clients {
        assert!(matches!(shutdown(client), VmmExitStatus::Ok));
    }
}
