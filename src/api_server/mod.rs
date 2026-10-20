mod service;

use std::path::{Path, PathBuf};
use std::thread;

use anyhow::{Context, Result, anyhow};
use hyper_uds::UnixListener;
use logger::{error_unlimited, info_unlimited};
use vmm::VmmClient;

pub fn start(api_sock: impl AsRef<Path>, vmm_client: VmmClient) -> Result<ApiServer> {
    let api_sock = api_sock.as_ref();
    let listener = UnixListener::bind(api_sock).context("bind listener")?;
    let _remove_api_sock_guard = RemoveApiSockGuard(api_sock.to_path_buf());

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let shutdown_fut = async {
        let _ = shutdown_rx.await;
    };

    let service = move |req| {
        let vmm_client = vmm_client.clone();
        async move { Ok(service::service(vmm_client, req).await) }
    };

    let task = async move {
        let res = hyper_uds::server::serve(listener, service, shutdown_fut)
            .await
            .context("api server serve");

        match res {
            Ok(_) => info_unlimited!("API server shutdown gracefully"),
            Err(e) => error_unlimited!("API server shutdown with error: {:?}", e),
        }
    };

    let join_handle = thread::Builder::new()
        .name("api-server".into())
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed building the Runtime")
                .block_on(task)
        })
        .context("spawn api server thread")?;

    Ok(ApiServer {
        _remove_api_sock_guard,
        shutdown_tx,
        join_handle,
    })
}

pub struct ApiServer {
    _remove_api_sock_guard: RemoveApiSockGuard,
    shutdown_tx: oneshot::Sender<()>,
    join_handle: thread::JoinHandle<()>,
}

impl ApiServer {
    pub async fn graceful_shutdown(self) -> Result<()> {
        let _ = self.shutdown_tx.send(());
        tokio::task::spawn_blocking(|| self.join_handle.join())
            .await
            .map_err(|_| anyhow!("join api server thread"))?
            .map_err(|_| anyhow!("join tokio blocking task"))?;

        Ok(())
    }
}
struct RemoveApiSockGuard(PathBuf);

impl Drop for RemoveApiSockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
