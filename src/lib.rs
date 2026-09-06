mod build;
pub mod cli;
pub mod init;
mod config;

use anyhow::{Context, Result};
use logger::{error_unrestricted, info_unrestricted};
use vmm::VmmClient;

pub use crate::build::*;
use crate::cli::{Cli, SubCommand};
use crate::init::init;

#[tokio::main(flavor = "local")]
pub async fn main() -> Result<()> {
    // Parse CLI arguments.
    // let cfg = config::parse_cli();
    let cli = Cli::parse_custom();

    // Run subcommands that do not require starting a VMM.
    if let Some(cmd) = cli.subcommand() {
        return run_subcommand(cmd).await;
    }

    // Setup environment, including signal handling, logging, etc.
    let (_flush_guard, shutdown_signal) = init(&cli).context("init vmm environment")?;

    info_unrestricted!("Dragonball starting, version = {VERSION_LONG}, cli = {cli}");

    // Spawn a new thread to start vmm.
    let vmm_client = vmm::start(cli.id().into()).context("start vmm")?;

    // Shutdown vmm when receiving SIGINT or SIGTERM.
    shutdown_on_signal(shutdown_signal, vmm_client.clone());

    // Configure the VM, then boot or restore if required.

    // Start API server if the API socket path is provided.

    // Wait for the VMM thread to finish.
    match vmm_client.async_join_vmm_thread().await {
        Ok(exit_status) => exit_status.print_log(),
        Err(e) => error_unrestricted!("failed to join vmm thread: {e:?}"),
    }

    // Gracefully shutdown the API server if it was started.

    // Flush metrics.

    Ok(())
}

fn shutdown_on_signal(
    shutdown_signal: impl Future<Output = ()> + Send + 'static,
    vmm_client: VmmClient,
) {
    tokio::task::spawn_local(async move {
        shutdown_signal.await;
        let _ = vmm_client.request_async(api::VmmRequest::Shutdown).await;
    });
}

async fn run_subcommand(_cmd: &SubCommand) -> Result<()> {
    Ok(())
}
