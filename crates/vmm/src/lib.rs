mod process_ctx;
mod vmm_server;
mod lifecycle;

use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use tokio::sync::mpsc;

pub use crate::process_ctx::ProcessCtx;
pub use crate::vmm_server::VmmClient;
use crate::vmm_server::VmmServer;
pub use crate::lifecycle::VmmExitStatus;

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
    todo!()
}
