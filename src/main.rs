use anyhow::{Context, Result};
use dragonball_evolved::cli::ParsedConfig;
use dragonball_evolved::{VERSION, VERSION_LONG, api_server, cli, cmds, init};
use logger::{error_unlimited, info_unlimited};
use vmm::{KvmContext, ProcessCtx};

#[tokio::main(flavor = "local")]
async fn main() -> Result<()> {
    // Parse CLI arguments.
    let (mut cfg, subcommand) = cli::parse().context("parse config from cli")?;

    // Run subcommands that do not require starting a VMM.
    if let Some(cmd) = subcommand {
        return cmds::run_subcommand(cmd).await;
    }

    // Setup environment, including signal handling, logging, etc.
    let (_flush_guard, shutdown_signal) = init::init(&cfg).context("init vmm environment")?;

    info_unlimited!("Dragonball starting, version = {VERSION_LONG}, config = {cfg:?}");

    // Spawn a new thread to start vmm.
    let vmm_client = vmm::start(make_process_ctx(&cfg)?).context("start vmm")?;

    // Shutdown vmm when receiving SIGINT or SIGTERM.
    let vmm_client2 = vmm_client.clone();
    tokio::task::spawn_local(async move {
        shutdown_signal.await;
        let _ = vmm_client2.call_async(api::lifecycle::Shutdown).await;
    });

    if let Err(error) = configure(&vmm_client, std::mem::take(&mut cfg.commands)).await {
        let _ = vmm_client.call_async(api::lifecycle::Shutdown).await;
        let _ = vmm_client.async_join_vmm_thread().await;
        return Err(error);
    }

    // Start API server if the API socket path is provided.
    let api_server = if let Some(sock) = &cfg.dragonball.api_sock {
        info_unlimited!("API server started, listening on {}", sock.display());
        Some(api_server::start(sock, vmm_client.clone()).context("start api server")?)
    } else {
        None
    };

    // Wait for the VMM thread to finish.
    let exit_result = vmm_client.async_join_vmm_thread().await;
    match &exit_result {
        Ok(exit_status) => exit_status.print_log(),
        Err(e) => error_unlimited!("failed to join vmm thread: {e:?}"),
    }

    // Gracefully shutdown the API server if it was started.
    if let Some(api_server) = api_server {
        api_server
            .graceful_shutdown()
            .await
            .context("api server graceful shutdown")?;
    }

    match exit_result? {
        vmm::VmmExitStatus::Ok => Ok(()),
        vmm::VmmExitStatus::Error(error) => anyhow::bail!("{error}"),
    }
}

async fn configure(client: &vmm::VmmClient, commands: Vec<api::VmmCommand>) -> Result<()> {
    let auto_start = commands
        .iter()
        .any(|command| matches!(command, api::VmmCommand::SetKernel(_, ())));
    for command in commands {
        let operation = format!("{command:?}");
        let (request, waiter) = command.into_request();
        client.send(request)?;
        waiter
            .await
            .map_err(|error| anyhow::anyhow!("{operation}: {error}"))?;
    }
    if auto_start {
        client
            .call_async(api::lifecycle::Start)
            .await?
            .map_err(|error| anyhow::anyhow!("Start: {error}"))?;
    }
    Ok(())
}

fn make_process_ctx(cfg: &api::config::Config) -> Result<ProcessCtx> {
    Ok(ProcessCtx {
        id: cfg.id().to_owned(),
        version: VERSION.to_owned(),
        kvm_ctx: KvmContext::from_dev_path(cfg.kvm_dev()).context("init kvm context")?,
        update_logger: |cfg| {
            logger_backend::update(logger_backend::Config::from_api(cfg, None))
                .map_err(api::ApiError::from)
        },
    })
}
