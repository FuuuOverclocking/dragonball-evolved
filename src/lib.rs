pub mod cli;
pub mod init;

use anyhow::{Context, Result};
use logger::{error_unrestricted, info_unrestricted};
use vmm::VmmClient;

use crate::cli::{Cli, CliOps, SubCommand};
use crate::init::init;

pub const VERSION: &str = env!("VERSION");
pub const VERSION_LONG: &str = env!("VERSION_LONG");
pub const COMMIT: &str = env!("GIT_SHORT_HASH");

#[tokio::main(flavor = "local")]
pub async fn main() -> Result<()> {
    // Parse CLI arguments.
    let cli = Cli::parse_custom();

    // Run subcommands that do not require starting a VMM.
    if let Some(cmd) = cli.subcommand() {
        return run_subcommand(cmd).await;
    }

    // Setup environment, including signal handling, logging, etc.
    let (_flush_guard, shutdown_signal) = init(&cli).context("init vmm environment")?;

    info_unrestricted!("Dragonball starting, version = {VERSION_LONG}, cli = {cli:?}");

    // Spawn a new thread to start vmm.
    let vmm_client = vmm::start(cli.id().into()).context("start vmm")?;

    // Shutdown vmm when receiving SIGINT or SIGTERM.
    shutdown_on_signal(shutdown_signal, vmm_client.clone());

    // Configure the VM, then boot or restore if required.
    if let Some(disk_path) = cli.poc_disk() {
        let report = vmm_client
            .request_async(|reply| {
                api::VmmRequest::StartPoc(
                    api::PocConfig {
                        disk_path: disk_path.to_owned(),
                    },
                    reply,
                )
            })
            .await
            .context("wait for KVM POC")?
            .context("run KVM POC")?;
        info_unrestricted!(
            "KVM POC completed: used_index={}, status={}, irq_status={:#x}, data={:02x?}",
            report.used_index,
            report.request_status,
            report.interrupt_status,
            report.data
        );
    }

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
