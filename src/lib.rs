mod config;
mod init;

use anyhow::{Context, Result};
use api::{Shutdown, VmmRequest};
use logger::{error_unlimited, info_unlimited};
use vmm::VmmClient;

use crate::config::SubCommand;
use crate::init::init;

pub const VERSION: &str = env!("VERSION");
pub const VERSION_LONG: &str = env!("VERSION_LONG");
pub const COMMIT: &str = env!("GIT_SHORT_HASH");

#[tokio::main(flavor = "local")]
pub async fn main() -> Result<()> {
    // Parse CLI arguments.
    let (cfg, subcommand) = config::parse_cli().context("parse config from cli")?;

    // Run subcommands that do not require starting a VMM.
    if let Some(cmd) = subcommand {
        return run_subcommand(cmd).await;
    }

    // Setup environment, including signal handling, logging, etc.
    let (_flush_guard, shutdown_signal) = init(&cfg).context("init vmm environment")?;

    info_unlimited!("Dragonball starting, version = {VERSION_LONG}, config = {cfg:?}");

    // Spawn a new thread to start vmm.
    let vmm_client = vmm::start(cfg.dragonball.id().into()).context("start vmm")?;

    // Shutdown vmm when receiving SIGINT or SIGTERM.
    shutdown_on_signal(shutdown_signal, vmm_client.clone());

    // Configure the VM, then boot or restore if required.

    // Start API server if the API socket path is provided.

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
        let _ = vmm_client
            .request_async(|reply| VmmRequest::Shutdown(Shutdown {}, reply))
            .await;
    });
}

async fn run_subcommand(_cmd: SubCommand) -> Result<()> {
    Ok(())
}

#[cfg(all(test, feature = "wire"))]
mod wire_tests {
    //! What the generated adapter actually feeds: a prepared command crosses into a
    //! live vmm through `enqueue`, and its erased reply comes back as JSON.

    use std::time::Duration;

    use api::wire::ReplyError;
    use api::{Command, GetInstanceInfo, Shutdown, Status};
    use serde_json::{Value, json};
    use tokio::time::timeout;
    use vmm::{VmmClient, VmmExitStatus};

    /// Every wait is bounded, so a broken bridge fails instead of hanging the run.
    const DEADLINE: Duration = Duration::from_secs(10);

    #[tokio::test(flavor = "current_thread")]
    async fn prepared_commands_reach_a_live_vmm_and_answer_as_json() {
        let client = vmm::start("wire-bridge".into()).expect("start vmm");

        // An empty vm has no devices to run, and the id is the one it started with.
        assert_eq!(
            enqueue_and_await(&client, Command::Status(Status {}, ())).await,
            json!({ "devices_running": 0 })
        );
        assert_eq!(
            enqueue_and_await(&client, Command::GetInstanceInfo(GetInstanceInfo {}, ())).await,
            json!({ "id": "wire-bridge" })
        );
        // `Shutdown` answers with `()`, which JSON holds as null.
        assert_eq!(
            enqueue_and_await(&client, Command::Shutdown(Shutdown {}, ())).await,
            Value::Null
        );

        join_vmm(&client).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn enqueue_refuses_once_the_vmm_has_joined_and_disconnects_its_waiter() {
        let client = vmm::start("wire-gone".into()).expect("start vmm");
        enqueue_and_await(&client, Command::Shutdown(Shutdown {}, ())).await;
        join_vmm(&client).await;

        let (request, waiter) = Command::Status(Status {}, ()).prepare();
        assert!(
            client.enqueue(request).is_err(),
            "a joined vmm takes no requests"
        );

        // The refused send dropped the request, so nothing can ever answer it.
        assert!(matches!(
            timeout(DEADLINE, waiter)
                .await
                .expect("the waiter settles before the deadline"),
            Err(ReplyError::Disconnected)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_shutdown_whose_waiter_is_dropped_still_ends_the_vmm() {
        let client = vmm::start("wire-abandoned".into()).expect("start vmm");

        let (request, waiter) = Command::Shutdown(Shutdown {}, ()).prepare();
        drop(waiter);
        client.enqueue(request).expect("enqueue");

        // Nobody is waiting, so only the thread finishing says the shutdown happened.
        join_vmm(&client).await;
    }

    async fn enqueue_and_await(client: &VmmClient, command: Command) -> Value {
        let (request, waiter) = command.prepare();
        client.enqueue(request).expect("enqueue");
        timeout(DEADLINE, waiter)
            .await
            .expect("a reply before the deadline")
            .expect("the vmm answered")
    }

    async fn join_vmm(client: &VmmClient) {
        let status = timeout(DEADLINE, client.async_join_vmm_thread())
            .await
            .expect("a join before the deadline")
            .expect("join vmm thread");
        assert!(
            matches!(status, VmmExitStatus::Ok),
            "unexpected exit status: {status:?}"
        );
    }
}
