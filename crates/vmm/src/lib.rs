pub mod devices;
mod exit_status;
mod vmm_server;

use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use api::{InstanceInfo, Reply, VmmRequest, VmmStatus};
use logger::{info_unlimited, warn_unlimited};
use tokio::select;
use tokio::sync::mpsc;

use crate::devices::{DeviceExit, DeviceManager};
pub use crate::exit_status::VmmExitStatus;
pub use crate::vmm_server::VmmClient;
use crate::vmm_server::VmmServer;

pub fn start(id: String) -> Result<VmmClient> {
    let instance_info = Arc::new(InstanceInfo { id });
    let (tx, rx) = mpsc::unbounded_channel();

    let thread_handle = thread::Builder::new()
        .name("vmm-main".into())
        .spawn({
            // The client and the loop answer "which instance is this" from the same
            // value, so they cannot disagree about it.
            let instance_info = Arc::clone(&instance_info);
            move || vmm_main(VmmServer(rx), instance_info)
        })
        .context("spawn vmm-main thread")?;

    Ok(VmmClient {
        thread_handle: Arc::new(Mutex::new(Some(thread_handle))),
        instance_info,
        tx,
    })
}

/// The vmm thread: its own runtime, its own devices, one request loop.
#[tokio::main(flavor = "local")]
async fn vmm_main(server: VmmServer, instance: Arc<InstanceInfo>) -> VmmExitStatus {
    run(server, instance, DeviceManager::new()).await
}

/// Serve requests until the vmm is asked to stop, then wind the devices down.
async fn run(
    mut server: VmmServer,
    instance: Arc<InstanceInfo>,
    mut devices: DeviceManager,
) -> VmmExitStatus {
    let (status, shutdown_reply) = loop {
        // The `select!` only decides *what happened*. Handling it afterwards is
        // what lets the request dispatch grow: this arm list stays three lines
        // long however many variants `VmmRequest` gains, and the handler gets
        // `&mut self`, which no arm body can have while the other futures are
        // still borrowing fields of `self`.
        let event = select! {
            biased;
            request = server.next_request() => Event::Request(request),
            Some(exit) = devices.next_exit() => Event::DeviceExit(exit),
        };

        let step = match event {
            Event::Request(Some(request)) => handle_request(request, &mut devices, &instance).await,

            // Nothing can ask the vmm to do anything ever again. The vmm itself did
            // not fail, so this is a clean exit, but an embedder dropping its last
            // handle mid-run is worth saying out loud.
            Event::Request(None) => {
                warn_unlimited!("Every vmm client is gone, shutting down");
                Step::stop(VmmExitStatus::Ok)
            }

            Event::DeviceExit(exit) => {
                match exit.result {
                    Ok(()) => info_unlimited!("Device {} stopped", exit.name),
                    Err(e) => warn_unlimited!("Device {} failed: {e:?}", exit.name),
                }
                Step::Continue
            }
        };

        match step {
            Step::Continue => {}
            Step::Stop { status, reply } => break (status, reply),
        }
    };

    // Devices stop at a point of their own choosing, so an in-flight request is
    // never torn in half.
    devices.quiesce().await;

    // Answering only now makes the reply mean "shutdown finished".
    if let Some(reply) = shutdown_reply {
        reply.send(());
    }
    status
}

async fn handle_request(
    request: VmmRequest,
    devices: &mut DeviceManager,
    instance: &InstanceInfo,
) -> Step {
    match request {
        VmmRequest::Shutdown(_, reply) => {
            info_unlimited!("Shutdown requested");
            Step::Stop {
                status: VmmExitStatus::Ok,
                reply: Some(reply),
            }
        }
        VmmRequest::Status(_, reply) => {
            reply.send(VmmStatus {
                devices_running: devices.running(),
            });
            Step::Continue
        }
        VmmRequest::GetInstanceInfo(_, reply) => {
            reply.send(instance.clone());
            Step::Continue
        }
    }
}

/// Something the vmm has to react to.
enum Event {
    /// A client request, or `None` once every client has gone.
    Request(Option<VmmRequest>),
    DeviceExit(DeviceExit),
}

/// What the main loop should do next.
enum Step {
    /// Keep serving.
    Continue,
    /// Wind down, then answer `reply` once every device has stopped.
    Stop {
        status: VmmExitStatus,
        reply: Option<Reply<()>>,
    },
}

impl Step {
    /// Wind down without anyone waiting to be told.
    fn stop(status: VmmExitStatus) -> Self {
        Self::Stop {
            status,
            reply: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use api::{Shutdown, reply_channel};

    use super::*;

    fn instance(id: &str) -> Arc<InstanceInfo> {
        Arc::new(InstanceInfo { id: id.into() })
    }

    /// Only reachable from inside the crate: a `VmmClient` owns both the request
    /// sender and the thread handle, so an integration test cannot disconnect the
    /// channel and still be able to join.
    #[test]
    fn the_loop_exits_once_every_client_is_gone() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(tx);

        // Nothing will ever arrive, so the loop has to notice rather than wait.
        let status = vmm_main(VmmServer(rx), instance("gone"));
        assert!(
            matches!(status, VmmExitStatus::Ok),
            "unexpected exit status: {status:?}"
        );
    }

    #[tokio::test(flavor = "local")]
    async fn a_dropped_answer_does_not_cancel_the_shutdown() {
        let (tx, rx) = mpsc::unbounded_channel();
        let devices = DeviceManager::new();
        let vmm = tokio::task::spawn_local(run(VmmServer(rx), instance("abandoned"), devices));

        let (reply, answer) = reply_channel();
        tx.send(VmmRequest::Shutdown(Shutdown {}, reply))
            .expect("send shutdown");
        drop(answer);

        assert!(matches!(vmm.await.expect("vmm task"), VmmExitStatus::Ok));
    }

    /// A shutdown reply has to mean the devices are stopped, not that the request
    /// was noticed. The device task parks on a gate this test controls, so the
    /// ordering is observed rather than raced: while the gate is shut, no reply can
    /// be in the channel unless the loop answered too early.
    #[tokio::test(flavor = "local")]
    async fn the_shutdown_reply_waits_for_devices_to_stop() {
        let (gate, opened) = tokio::sync::oneshot::channel();
        let (parked, reached_gate) = tokio::sync::oneshot::channel();
        let stopped = Arc::new(AtomicBool::new(false));

        let mut devices = DeviceManager::new();
        let mut stop = devices.stop_signal();
        let flag = Arc::clone(&stopped);
        devices.spawn("gated", async move {
            stop.requested().await;
            let _ = parked.send(());
            let _ = opened.await;
            flag.store(true, Ordering::SeqCst);
            Ok(())
        });

        let (tx, rx) = mpsc::unbounded_channel();
        let vmm = tokio::task::spawn_local(run(VmmServer(rx), instance("gated"), devices));

        let (reply, answer) = reply_channel();
        tx.send(VmmRequest::Shutdown(Shutdown {}, reply))
            .expect("send shutdown");

        reached_gate
            .await
            .expect("the device task should reach its gate");
        assert!(
            matches!(answer.try_recv(), Err(oneshot::TryRecvError::Empty)),
            "shutdown was answered before the devices stopped"
        );

        let _ = gate.send(());
        answer.await.expect("shutdown should be answered");
        assert!(stopped.load(Ordering::SeqCst));
        assert!(matches!(vmm.await.expect("vmm task"), VmmExitStatus::Ok));
    }
}
