mod exit_status;
mod kvm_context;
mod lifecycle;
mod process_ctx;
mod spec;
mod vmm_server;

use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use tokio::sync::mpsc;

pub use crate::exit_status::VmmExitStatus;
use crate::lifecycle::Controller;
pub use crate::process_ctx::ProcessCtx;
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
    let mut controller = Controller::new(MachineSpec::default());

    todo!()
}
