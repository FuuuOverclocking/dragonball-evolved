pub mod devices;
mod exit_status;
mod instance_info;
mod vmm_server;

use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use api::{Reply, VmmRequest, VmmStatus};
use logger::{info_unrestricted, warn_unrestricted};
use tokio::select;
use tokio::sync::mpsc;

use crate::devices::{DeviceExit, DeviceManager};
pub use crate::exit_status::VmmExitStatus;
pub use crate::instance_info::InstanceInfo;
use crate::vmm_server::Requests;
pub use crate::vmm_server::VmmClient;

pub fn start(id: String) -> Result<VmmClient> {
    let instance_info = Arc::new(InstanceInfo { id });
    let (tx, rx) = mpsc::unbounded_channel();

    let thread_handle = thread::Builder::new()
        .name("vmm-main".into())
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build_local(Default::default())
                .expect("Failed building the tokio runtime for vmm-main")
                .block_on(vmm_main(Requests(rx)))
        })
        .context("spawn vmm-main thread")?;

    Ok(VmmClient {
        thread_handle: Arc::new(Mutex::new(Some(thread_handle))),
        instance_info,
        tx,
    })
}

/// The vmm's main loop.
async fn vmm_main(mut requests: Requests) -> VmmExitStatus {
    let mut devices = DeviceManager::new();

    let (status, shutdown_reply) = loop {
        // The `select!` only decides *what happened*. Handling it afterwards is
        // what lets the request dispatch grow: this arm list stays three lines
        // long however many variants `VmmRequest` gains, and the handler gets
        // `&mut self`, which no arm body can have while the other futures are
        // still borrowing fields of `self`.
        let event = select! {
            biased;
            request = requests.next() => Event::Request(request),
            Some(exit) = devices.next_exit() => Event::DeviceExit(exit),
        };

        let step = match event {
            Event::Request(Some(request)) => handle_request(request, &mut devices).await,

            // Nothing can ask the vmm to do anything ever again. The vmm itself did
            // not fail, so this is a clean exit, but an embedder dropping its last
            // handle mid-run is worth saying out loud.
            Event::Request(None) => {
                warn_unrestricted!("Every vmm client is gone, shutting down");
                Step::stop(VmmExitStatus::Ok)
            }

            Event::DeviceExit(exit) => {
                match exit.result {
                    Ok(()) => info_unrestricted!("Device {} stopped", exit.name),
                    Err(e) => warn_unrestricted!("Device {} failed: {e:?}", exit.name),
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

async fn handle_request(request: VmmRequest, devices: &mut DeviceManager) -> Step {
    match request {
        VmmRequest::Shutdown(reply) => {
            info_unrestricted!("Shutdown requested");
            Step::Stop {
                status: VmmExitStatus::Ok,
                reply: Some(reply),
            }
        }
        VmmRequest::Status(reply) => {
            reply.send(VmmStatus {
                devices_running: devices.running(),
            });
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
    use super::*;

    /// Only reachable from inside the crate: a `VmmClient` owns both the request
    /// sender and the thread handle, so an integration test cannot disconnect the
    /// channel and still be able to join.
    #[tokio::test(flavor = "local")]
    async fn the_loop_exits_once_every_client_is_gone() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(tx);

        // Nothing will ever arrive, so the loop has to notice rather than wait.
        let status = vmm_main(Requests(rx)).await;
        assert!(
            matches!(status, VmmExitStatus::Ok),
            "unexpected exit status: {status:?}"
        );
    }
}
