pub mod devices;
mod exit_status;
mod vmm_server;

use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use api::instance_info::InstanceInfo;
use api::{Reply, VmmRequest};
use logger::{info_unlimited, warn_unlimited};
use tokio::select;
use tokio::sync::mpsc;

use crate::devices::{DeviceExit, DeviceManager};
pub use crate::exit_status::VmmExitStatus;
pub use crate::vmm_server::VmmClient;
use crate::vmm_server::VmmServer;

pub fn start(info: InstanceInfo) -> Result<VmmClient> {
    let (tx, rx) = mpsc::unbounded_channel();

    let thread_handle = thread::Builder::new()
        .name("vmm-main".into())
        .spawn(move || vmm_main(VmmServer(rx), info))
        .context("spawn vmm-main thread")?;

    Ok(VmmClient {
        thread_handle: Arc::new(Mutex::new(Some(thread_handle))),
        tx,
    })
}

#[tokio::main(flavor = "local")]
async fn vmm_main(mut server: VmmServer, mut info: InstanceInfo) -> VmmExitStatus {
    let mut devices = DeviceManager::default();

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
            Event::Request(Some(request)) => handle_request(request, &mut devices).await,

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

async fn handle_request(request: VmmRequest, devices: &mut DeviceManager) -> Step {
    // match request {
    // }

    todo!()
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
