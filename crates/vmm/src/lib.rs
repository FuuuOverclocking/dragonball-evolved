pub mod arch;
pub mod devices;
mod exit_status;
mod kvm_context;
mod lifecycle;
pub mod machine;
mod process_ctx;
pub mod resources;
pub mod spec;
mod vmm_server;

use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use tokio::select;
use tokio::sync::mpsc;

pub use crate::exit_status::VmmExitStatus;
pub use crate::kvm_context::{HostCapabilities, KvmContext};
use crate::lifecycle::{BusyCtx, BusyEvent, Controller, Event, Step, Vm, handle_busy, handle_idle};
pub use crate::process_ctx::ProcessCtx;
use crate::spec::MachineSpec;
pub use crate::vmm_server::VmmClient;
use crate::vmm_server::{Inbound, VmmServer};

pub fn start(ctx: ProcessCtx) -> Result<VmmClient> {
    let (tx, rx) = mpsc::unbounded_channel();

    let thread_handle = thread::Builder::new()
        .name("vmm-main".into())
        .spawn(move || vmm_main(VmmServer::new(rx), ctx))
        .context("spawn vmm-main thread")?;

    Ok(VmmClient {
        thread_handle: Arc::new(Mutex::new(Some(thread_handle))),
        tx,
    })
}

#[tokio::main(flavor = "local")]
async fn vmm_main(mut server: VmmServer, ctx: ProcessCtx) -> VmmExitStatus {
    let mut controller = Controller::new(MachineSpec::default());

    let mut status = loop {
        match controller {
            Controller::Idle(mut vm) => {
                // A Start that failed after every client left: nobody can start it again.
                if server.is_detached() && matches!(vm, Vm::NotStarted(_)) {
                    controller = Controller::Idle(vm);
                    break VmmExitStatus::Ok;
                }
                // IDLE: select machine/device events (biased) and requests.
                let event = select! {
                    biased;
                    event = vm.next_event() => event,
                    inbound = server.next_inbound() => match inbound {
                        Inbound::Request(request) => Event::Request(request),
                        Inbound::Detached => Event::Detached,
                    },
                };
                let (next, step) = handle_idle(vm, event, &ctx).await;
                match step {
                    Step::Continue => controller = next,
                    Step::Stop(s) => {
                        controller = next;
                        break s;
                    }
                }
            }
            Controller::Busy {
                stable,
                mut operation,
            } => {
                let event = select! {
                    biased;
                    completion = &mut operation.future => BusyEvent::Completion(Box::new(completion)),
                    inbound = server.next_inbound() => match inbound {
                        Inbound::Request(request) => BusyEvent::Request(request),
                        Inbound::Detached => BusyEvent::Detached,
                    },
                };
                let (next, step) =
                    handle_busy(stable, operation, event, &BusyCtx::from(&ctx)).await;
                match step {
                    Step::Continue => controller = next,
                    Step::Stop(s) => {
                        controller = next;
                        break s;
                    }
                }
            }
        }
    };

    drop(server);

    if let Err(error) = controller.teardown().await {
        status = VmmExitStatus::Error(format!("teardown: {error:#}"));
    }
    status
}
