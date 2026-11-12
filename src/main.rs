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

    // Spawn a new thread to start VMM.
    let vmm_client = vmm::start(make_process_ctx(&cfg)?).context("start vmm")?;

    // Shutdown VMM when receiving SIGINT or SIGTERM.
    let vmm_client2 = vmm_client.clone();
    tokio::task::spawn_local(async move {
        shutdown_signal.await;
        vmm_client2
            .call_drop_response(api::lifecycle::ShutdownVmm)
            .ok();
    });

    // Configure VMM (start if boot source is given). Then start API server if `api_sock` provided.
    let setup = async {
        configure_vmm(&vmm_client, std::mem::take(&mut cfg.commands)).await?;
        if let Some(sock) = cfg.dragonball.api_sock.as_ref() {
            let server = api_server::start(sock, vmm_client.clone()).context("start api server")?;
            return Ok(Some(server));
        }
        Ok(None)
    };
    let api_server = match setup.await {
        Ok(s) => s,
        Err(e) => {
            vmm_client
                .call_drop_response(api::lifecycle::ShutdownVmm)
                .ok();
            vmm_client
                .join_vmm_thread_async()
                .await
                .expect("Failed to join vmm thread");
            return Err(e);
        }
    };

    // Wait for the VMM thread to finish.
    let exit_status = vmm_client
        .join_vmm_thread_async()
        .await
        .expect("Failed to join VMM thread");

    // Gracefully shutdown the API server if it was started.
    if let Some(api_server) = api_server {
        if let Err(e) = api_server.graceful_shutdown().await {
            error_unlimited!("Error during API server graceful shutdown: {e}")
        }
    }

    info_unlimited!("VMM exited with {exit_status:?}");
    exit_status
}

fn make_process_ctx(cfg: &api::config::Config) -> Result<ProcessCtx> {
    Ok(ProcessCtx {
        id: cfg.id().to_owned(),
        version: VERSION.to_owned(),
        kvm_ctx: KvmContext::from_dev_path(cfg.kvm_dev()).context("init kvm context")?,
        update_logger: |cfg| {
            logger_backend::update(logger_backend::Config::from_api(cfg, None)).map_err(Into::into)
        },
    })
}

async fn configure_vmm(client: &vmm::VmmClient, commands: Vec<api::VmmCommand>) -> Result<()> {
    let auto_start = commands
        .iter()
        .any(|command| matches!(command, api::VmmCommand::SetBoot(_, ())));
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
