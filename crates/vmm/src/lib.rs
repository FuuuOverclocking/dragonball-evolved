pub mod arch;
pub mod devices;
mod exit_status;
mod kvm_context;
mod lifecycle;
pub mod machine;
mod process_ctx;
pub mod spec;
mod vmm_server;

use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use tokio::select;
use tokio::sync::mpsc;

pub use crate::exit_status::VmmExitStatus;
pub use crate::kvm_context::{HostCapabilities, KvmContext};
use crate::lifecycle::{Event, Step, Vm};
pub use crate::process_ctx::ProcessCtx;
use crate::spec::MachineSpec;
pub use crate::vmm_server::VmmClient;
use crate::vmm_server::VmmServer;

pub fn start(ctx: ProcessCtx) -> Result<VmmClient> {
    let (tx, rx) = mpsc::unbounded_channel();

    let thread_handle = thread::Builder::new()
        .name("vmm-main".into())
        .spawn(move || vmm_main(VmmServer(rx), ctx))
        .context("spawn vmm-main thread")?;

    Ok(VmmClient {
        thread_handle: Arc::new(Mutex::new(Some(thread_handle))),
        tx,
    })
}

#[tokio::main(flavor = "local")]
async fn vmm_main(mut server: VmmServer, ctx: ProcessCtx) -> VmmExitStatus {
    let mut vm = Vm::NotStarted(MachineSpec::default());

    let (vm, mut status) = loop {
        let event = select! {
            biased;
            event = vm.next_event() => event,
            request = server.next_request() => Event::Request(request),
        };
        match vm.handle(event, &ctx).await {
            (next, Step::Continue) => vm = next,
            (next, Step::Stop(status)) => break (next, status),
        }
    };

    drop(server);
    
    if let Err(error) = vm.teardown().await {
        status = VmmExitStatus::Error(format!("teardown: {error:#}"));
    }
    status
}
