use anyhow::{Context, Result};
use api::VmmRequest;
use api::instance_info::InstanceInfo;
use dragonball_evolved::cli::ParsedConfig;
use dragonball_evolved::{VERSION, VERSION_LONG, cli, cmds, init};
use logger::{error_unlimited, info_unlimited};
use vmm::VmmClient;

#[tokio::main(flavor = "local")]
async fn main() -> Result<()> {
    // Parse CLI arguments.
    let (cfg, subcommand) = cli::parse().context("parse config from cli")?;

    // Run subcommands that do not require starting a VMM.
    if let Some(cmd) = subcommand {
        return cmds::run_subcommand(cmd).await;
    }

    // Setup environment, including signal handling, logging, etc.
    let (_flush_guard, shutdown_signal) = init::init(&cfg).context("init vmm environment")?;

    info_unlimited!("Dragonball starting, version = {VERSION_LONG}, config = {cfg:?}");

    // Spawn a new thread to start vmm.
    let vmm_client = vmm::start(InstanceInfo {
        id: cfg.id().into(),
        version: VERSION.into(),
        ..Default::default()
    })
    .context("start vmm")?;

    // Shutdown vmm when receiving SIGINT or SIGTERM.
    shutdown_on_signal(shutdown_signal, vmm_client.clone());

    // Configure the VM, then boot or restore if required.

    // Start API server if the API socket path is provided.
    if let Some(sock) = &cfg.dragonball.api_sock {
        info_unlimited!("API server started, listening on {}", sock.display());
    }

    // Wait for the VMM thread to finish.
    match vmm_client.async_join_vmm_thread().await {
        Ok(exit_status) => exit_status.print_log(),
        Err(e) => error_unlimited!("failed to join vmm thread: {e:?}"),
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
        // let _ = vmm_client
        //     .request_async(|reply| VmmRequest::Shutdown(Shutdown {}, reply))
        //     .await;
    });
}
