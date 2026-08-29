//! The vmm thread's async main loop and its request dispatch.

use std::time::Duration;

use api::{GetInstanceInfo, InstanceInfo, Shutdown, Status, VmmRequest, VmmStatus};
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
        .request_timeout(
            |reply| VmmRequest::Shutdown(Shutdown {}, reply),
            REPLY_TIMEOUT,
        )
        .expect("shutdown request should be answered");

    client.join_vmm_thread().expect("join vmm thread")
}

fn instance_info(client: &VmmClient) -> InstanceInfo {
    client
        .request_timeout(
            |reply| VmmRequest::GetInstanceInfo(GetInstanceInfo {}, reply),
            REPLY_TIMEOUT,
        )
        .expect("instance info request should be answered")
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
        .request_timeout(|reply| VmmRequest::Status(Status {}, reply), REPLY_TIMEOUT)
        .expect("status request should be answered");
    // An empty vm has no devices to run.
    assert_eq!(status.devices_running, 0);

    // A non-terminal request leaves the loop serving, so shutdown still works.
    assert!(matches!(shutdown(&client), VmmExitStatus::Ok));
}

#[test]
fn the_vmm_answers_for_the_instance_the_client_holds() {
    let client = start("instance-info-test");

    let info = instance_info(&client);
    assert_eq!(
        info,
        InstanceInfo {
            id: "instance-info-test".into()
        }
    );
    assert_eq!(info, *client.instance_info());

    assert!(matches!(shutdown(&client), VmmExitStatus::Ok));
}

#[test]
fn several_vmms_run_side_by_side_in_one_process() {
    // Nothing about a vmm is process-global. Instance info used to live in a
    // `OnceLock`, which made the second `start` fail and forced every test that
    // needs a live vmm into a test binary of its own.
    let clients: Vec<_> = (0..3).map(|i| start(&format!("vmm-{i}"))).collect();

    // Each loop answers for its own instance, so a crossed wire shows up here.
    for (i, client) in clients.iter().enumerate() {
        let expected = InstanceInfo {
            id: format!("vmm-{i}"),
        };
        assert_eq!(client.instance_info(), &expected);
        assert_eq!(instance_info(client), expected);
    }
    for client in &clients {
        assert!(matches!(shutdown(client), VmmExitStatus::Ok));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn requests_can_be_awaited_by_an_async_caller() {
    let client = start("async-test");

    let status: VmmStatus = client
        .request_async(|reply| VmmRequest::Status(Status {}, reply))
        .await
        .expect("status request should be answered");
    assert_eq!(status.devices_running, 0);

    let info: InstanceInfo = client
        .request_async(|reply| VmmRequest::GetInstanceInfo(GetInstanceInfo {}, reply))
        .await
        .expect("instance info request should be answered");
    assert_eq!(info.id, "async-test");

    client
        .request_async(|reply| VmmRequest::Shutdown(Shutdown {}, reply))
        .await
        .expect("shutdown request should be answered");
    let status = client
        .async_join_vmm_thread()
        .await
        .expect("join vmm thread");
    assert!(
        matches!(status, VmmExitStatus::Ok),
        "unexpected exit status: {status:?}"
    );
}

#[test]
fn a_shutdown_nobody_waits_for_still_finishes() {
    let client = start("abandoned");

    // Giving up on the answer is the caller's business: the vmm winds down anyway,
    // and answering into a dropped channel is not a failure it can report.
    let _ = client.request_timeout(
        |reply| VmmRequest::Shutdown(Shutdown {}, reply),
        Duration::ZERO,
    );

    let status = client.join_vmm_thread().expect("join vmm thread");
    assert!(
        matches!(status, VmmExitStatus::Ok),
        "unexpected exit status: {status:?}"
    );
}
