pub mod align;
pub mod arch;
pub mod bus;
pub mod devices;
mod error;
mod exit_status;
mod kvm_context;
pub mod machine;
mod main_loop;
pub mod memory;
mod process_ctx;
pub mod resources;
pub mod space;
pub mod spec;
pub mod vm;
mod vmm_server;

use std::{io, thread};

use tokio::runtime::{Builder, LocalOptions};
use tokio::sync::mpsc;

pub use crate::exit_status::VmmExitStatus;
pub use crate::kvm_context::{HostCapabilities, KvmContext, KvmContextError};
pub use crate::process_ctx::ProcessCtx;
use crate::vmm_server::VmmServer;
pub use crate::vmm_server::{CallError, JoinError, ServerStopped, VmmClient};

/// spawn the vmm-main thread
#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub struct StartError(#[source] io::Error);

/// build the vmm-main runtime
#[derive(Debug, displaydoc::Display, thiserror::Error)]
struct RuntimeError(#[source] io::Error);

pub fn start(ctx: ProcessCtx) -> Result<VmmClient, StartError> {
    let (tx, rx) = mpsc::unbounded_channel();
    let server = VmmServer::new(rx);
    let thread = thread::Builder::new()
        .name("vmm-main".into())
        .spawn(move || vmm_main(server, ctx))
        .map_err(StartError)?;
    Ok(VmmClient::new(tx, Some(thread)))
}

fn vmm_main(server: VmmServer, ctx: ProcessCtx) -> VmmExitStatus {
    match Builder::new_current_thread()
        .enable_all()
        .build_local(LocalOptions::default())
    {
        Ok(runtime) => runtime.block_on(main_loop::run(server, &ctx)),
        Err(error) => VmmExitStatus::Error(error::render_chain(&RuntimeError(error))),
    }
}
