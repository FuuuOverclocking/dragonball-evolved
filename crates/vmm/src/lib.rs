pub mod devices;
mod exit_status;
mod instance_info;
pub mod memory;
mod poc;
mod vcpu;
mod vm;
mod vmm_server;

use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use api::{PocError, PocReport, Reply, VmmRequest, VmmStatus};
use logger::{error_unrestricted, info_unrestricted, warn_unrestricted};
use tokio::select;
use tokio::sync::mpsc;

use crate::devices::{DeviceExit, DeviceManager};
pub use crate::exit_status::VmmExitStatus;
pub use crate::instance_info::InstanceInfo;
use crate::poc::PocMachine;
use crate::vcpu::VcpuExit;
pub use crate::vmm_server::VmmClient;
use crate::vmm_server::VmmServer;

pub fn start(id: String) -> Result<VmmClient> {
    let instance_info = Arc::new(InstanceInfo { id });
    let (tx, rx) = mpsc::unbounded_channel();
    let server = VmmServer { rx };

    let thread_handle = thread::Builder::new()
        .name("vmm-main".into())
        .spawn(move || vmm_main(server))
        .context("spawn vmm-main thread")?;

    Ok(VmmClient {
        thread_handle: Arc::new(Mutex::new(Some(thread_handle))),
        instance_info,
        tx,
    })
}

#[tokio::main(flavor = "local")]
async fn vmm_main(server: VmmServer) -> VmmExitStatus {
    match Vmm::new(server) {
        Ok(vmm) => vmm.run().await,
        Err(e) => {
            error_unrestricted!("Failed to set up the vmm: {e:?}");
            VmmExitStatus::Error(format!("{e:#}"))
        }
    }
}

/// State owned by the vmm thread.
struct Vmm {
    requests: VmmServer,
    devices: DeviceManager,
    vcpu_exit_tx: mpsc::UnboundedSender<VcpuExit>,
    vcpu_exits: mpsc::UnboundedReceiver<VcpuExit>,
    machine: Option<PocMachine>,
    poc_reply: Option<Reply<Result<PocReport, PocError>>>,
}

impl Vmm {
    fn new(server: VmmServer) -> Result<Self> {
        let (vcpu_exit_tx, vcpu_exits) = mpsc::unbounded_channel();
        Ok(Self {
            requests: server,
            devices: DeviceManager::new(),
            vcpu_exit_tx,
            vcpu_exits,
            machine: None,
            poc_reply: None,
        })
    }

    /// The vmm's main loop.
    ///
    /// This is what replaces firecracker's
    /// `loop { event_manager.run(); check shutdown_exit_code() }`. There is no
    /// central epoll set to pump, because devices spawned on
    /// [`DeviceManager`](devices::DeviceManager) wait on their own descriptors;
    /// what is left here are the events that belong to the vmm as a whole.
    ///
    /// Device attachment goes just before this loop, once a boot source or an api
    /// request has produced something to attach.
    async fn run(mut self) -> VmmExitStatus {
        let (status, shutdown_reply) = loop {
            // The `select!` only decides *what happened*. Handling it afterwards is
            // what lets the request dispatch grow: this arm list stays three lines
            // long however many variants `VmmRequest` gains, and the handler gets
            // `&mut self`, which no arm body can have while the other futures are
            // still borrowing fields of `self`.
            let event = select! {
                biased;
                request = self.requests.next() => Event::Request(request),
                Some(exit) = self.vcpu_exits.recv() => Event::VcpuExit(exit),
                Some(exit) = self.devices.next_exit() => Event::DeviceExit(exit),
            };

            match self.handle(event).await {
                Step::Continue => {}
                Step::Stop { status, reply } => break (status, reply),
            }
        };

        if let Some(machine) = self.machine.take()
            && let Err(e) = machine.stop()
        {
            error_unrestricted!("Failed to stop vCPU: {e:?}");
        }
        if let Some(reply) = self.poc_reply.take() {
            reply.send(Err(PocError::new("vmm stopped before the POC completed")));
        }

        // Devices stop at a point of their own choosing, so an in-flight request is
        // never torn in half.
        self.devices.quiesce().await;

        // Answering only now makes the reply mean "shutdown finished".
        if let Some(reply) = shutdown_reply {
            reply.send(());
        }
        status
    }

    async fn handle(&mut self, event: Event) -> Step {
        match event {
            Event::Request(Some(request)) => self.handle_request(request).await,

            // Nothing can ask the vmm to do anything ever again. The vmm itself did
            // not fail, so this is a clean exit, but an embedder dropping its last
            // handle mid-run is worth saying out loud.
            Event::Request(None) => {
                warn_unrestricted!("Every vmm client is gone, shutting down");
                Step::stop(VmmExitStatus::Ok)
            }

            Event::VcpuExit(VcpuExit::Completed) => {
                info_unrestricted!("vCPU observed the async device completion");
                match self.finish_poc() {
                    Ok(report) => {
                        if let Some(reply) = self.poc_reply.take() {
                            reply.send(Ok(report));
                        }
                        Step::stop(VmmExitStatus::Ok)
                    }
                    Err(e) => self.fail_poc(format!("finish POC: {e:#}")),
                }
            }
            Event::VcpuExit(VcpuExit::Shutdown) => self.fail_poc("guest requested shutdown".into()),
            Event::VcpuExit(VcpuExit::Stopped) => {
                self.fail_poc("vCPU stopped before the POC completed".into())
            }
            Event::VcpuExit(VcpuExit::Error(e)) => self.fail_poc(e),

            // A device finishing while the POC is active means the guest can no
            // longer make progress, so turn it into a terminal machine error.
            Event::DeviceExit(exit) if self.machine.is_some() => self.fail_poc(format!(
                "device {} exited early: {:?}",
                exit.name, exit.result
            )),
            Event::DeviceExit(exit) => {
                match exit.result {
                    Ok(()) => info_unrestricted!("Device {} stopped", exit.name),
                    Err(e) => warn_unrestricted!("Device {} failed: {e:?}", exit.name),
                }
                Step::Continue
            }
        }
    }

    /// One arm per request variant.
    ///
    /// Most handlers only compute a reply and return [`Step::Continue`]; only the
    /// few that decide the vmm's fate return [`Step::Stop`]. As variants are added,
    /// an arm that outgrows a couple of lines becomes a method call, keeping this a
    /// flat table of request to handler.
    ///
    /// Handlers run on the main loop, so a slow one delays the other event sources.
    /// A request that needs to do real work should move its [`Reply`] into a task
    /// and answer from there.
    async fn handle_request(&mut self, request: VmmRequest) -> Step {
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
                    devices_running: self.devices.running(),
                    vcpu_running: self.machine.is_some(),
                    memory_bytes: self.machine.as_ref().map_or(0, PocMachine::memory_bytes),
                });
                Step::Continue
            }
            VmmRequest::StartPoc(config, reply) => {
                if self.machine.is_some() {
                    reply.send(Err(PocError::new("a machine is already running")));
                    return Step::Continue;
                }

                match PocMachine::start(&config, &mut self.devices, self.vcpu_exit_tx.clone()) {
                    Ok(machine) => {
                        self.machine = Some(machine);
                        self.poc_reply = Some(reply);
                    }
                    Err(e) => reply.send(Err(PocError::new(format!("start POC: {e:#}")))),
                }
                Step::Continue
            }
        }
    }

    fn finish_poc(&mut self) -> Result<PocReport> {
        self.machine
            .take()
            .context("no machine exists for the vCPU exit")?
            .finish()
    }

    fn fail_poc(&mut self, message: String) -> Step {
        if let Some(reply) = self.poc_reply.take() {
            reply.send(Err(PocError::new(message.clone())));
        }
        Step::stop(VmmExitStatus::Error(message))
    }
}

/// Something the vmm has to react to.
enum Event {
    /// A client request, or `None` once every client has gone.
    Request(Option<VmmRequest>),
    VcpuExit(VcpuExit),
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
    #[test]
    fn the_loop_exits_once_every_client_is_gone() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(tx);

        // Nothing will ever arrive, so the loop has to notice rather than wait.
        let status = vmm_main(VmmServer { rx });
        assert!(
            matches!(status, VmmExitStatus::Ok),
            "unexpected exit status: {status:?}"
        );
    }
}
