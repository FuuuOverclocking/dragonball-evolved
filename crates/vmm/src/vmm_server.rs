use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use api::{RawVmmOp, VmmRequest};
use tokio::sync::mpsc;

use crate::VmmExitStatus;

/// The vmm thread's end of the request channel.
pub(crate) struct VmmServer(pub(crate) mpsc::UnboundedReceiver<VmmRequest>);

impl VmmServer {
    /// Wait for the next request, or `None` once every client has gone away.
    ///
    /// Cancel-safe, so it can sit directly in a `select!` arm.
    pub(crate) async fn next_request(&mut self) -> Option<VmmRequest> {
        self.0.recv().await
    }
}

/// Cloneable VMM request sender; clones share a single-use join handle and keep the request channel open.
#[derive(Clone)]
pub struct VmmClient {
    pub(crate) thread_handle: Arc<Mutex<Option<thread::JoinHandle<VmmExitStatus>>>>,
    pub(crate) tx: mpsc::UnboundedSender<VmmRequest>,
}

impl VmmClient {
    fn take_thread_handle(&self) -> Result<thread::JoinHandle<VmmExitStatus>> {
        self.thread_handle
            .lock()
            .map_err(|e| anyhow!("lock vmm thread handle: {e}"))?
            .take()
            .context("vmm thread handle was already taken")
    }

    /// Blocks for exit without requesting shutdown; errors on a poisoned/taken handle or a panicked VMM thread.
    pub fn join_vmm_thread(&self) -> Result<VmmExitStatus> {
        let handle = self.take_thread_handle()?;
        handle.join().map_err(|_| anyhow!("vmm thread panicked"))
    }

    /// Like [`Self::join_vmm_thread`] via Tokio's blocking pool; once started, cancellation forfeits the result.
    pub async fn async_join_vmm_thread(&self) -> Result<VmmExitStatus> {
        let handle = self.take_thread_handle()?;
        tokio::task::spawn_blocking(move || {
            handle.join().map_err(|_| anyhow!("vmm thread panicked"))
        })
        .await
        .context("join tokio blocking task")?
    }

    /// Hand a request that already carries its own reply channel to the vmm thread.
    ///
    /// Success says the request was queued and nothing more: whether the operation
    /// ran, and what it produced, is still only what its reply says. An error means
    /// the vmm is gone, so the request it took with it will never be answered.
    pub fn send(&self, request: VmmRequest) -> Result<()> {
        self.tx
            .send(request)
            .map_err(|_| anyhow!("vmm server is no longer running"))
    }

    /// Blocks for a typed reply; channel failures are outer errors, while operation results stay in `Resp`.
    pub fn call<Req, Resp>(&self, request: Req) -> Result<Resp>
    where
        Req: RawVmmOp<Response = Resp>,
    {
        let (op, rx) = request.into_vmm_request();
        self.send(op)?;
        rx.recv().context("receive vmm reply")
    }

    /// Like [`Self::call`], but limits reply waiting after enqueueing; timeout does not cancel the operation.
    pub fn call_timeout<Req, Resp>(&self, request: Req, timeout: Duration) -> Result<Resp>
    where
        Req: RawVmmOp<Response = Resp>,
    {
        let (op, rx) = request.into_vmm_request();
        self.send(op)?;
        rx.recv_timeout(timeout).context("receive vmm reply")
    }

    /// Like [`Self::call`] without blocking; dropping its future after enqueueing does not cancel the operation.
    pub async fn call_async<Req, Resp>(&self, request: Req) -> Result<Resp>
    where
        Req: RawVmmOp<Response = Resp>,
    {
        let (op, rx) = request.into_vmm_request();
        self.send(op)?;
        rx.await.context("receive vmm reply")
    }
}

#[cfg(test)]
mod tests {
    use api::block::AddDisk;
    use api::instance_info::{GetInstanceInfo, InstanceInfo, VmState};

    use super::*;

    fn client_server() -> (VmmClient, VmmServer) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            VmmClient {
                thread_handle: Arc::new(Mutex::new(None)),
                tx,
            },
            VmmServer(rx),
        )
    }

    fn instance_info() -> InstanceInfo {
        InstanceInfo {
            id: "test-vm".into(),
            version: "1.0".into(),
            state: VmState::Running,
        }
    }

    fn respond(request: VmmRequest) {
        match request {
            VmmRequest::GetInstanceInfo(_, reply) => reply.send(instance_info()),
            VmmRequest::AddDisk(_, reply) => reply.send(Err(api::ApiError::msg("disk failed"))),
            _ => panic!("unexpected request"),
        }
    }

    #[test]
    fn sync_calls_preserve_responses_and_operation_errors() {
        for with_timeout in [false, true] {
            let (client, mut server) = client_server();
            let worker = thread::spawn(move || {
                respond(server.0.blocking_recv().unwrap());
                respond(server.0.blocking_recv().unwrap());
            });
            let info = if with_timeout {
                client.call_timeout(GetInstanceInfo, Duration::from_secs(5))
            } else {
                client.call(GetInstanceInfo)
            };
            assert_eq!(info.unwrap(), instance_info());
            let result = if with_timeout {
                client.call_timeout(AddDisk {}, Duration::from_secs(5))
            } else {
                client.call(AddDisk {})
            };
            assert_eq!(result.unwrap(), Err(api::ApiError::msg("disk failed")));
            worker.join().unwrap();
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_calls_preserve_responses_and_operation_errors() {
        let (client, mut server) = client_server();
        let worker = tokio::spawn(async move {
            respond(server.next_request().await.unwrap());
            respond(server.next_request().await.unwrap());
        });
        assert_eq!(
            client.call_async(GetInstanceInfo).await.unwrap(),
            instance_info()
        );
        assert_eq!(
            client.call_async(AddDisk {}).await.unwrap(),
            Err(api::ApiError::msg("disk failed"))
        );
        worker.await.unwrap();
    }

    #[test]
    fn sync_calls_reject_a_stopped_server() {
        let (client, server) = client_server();
        drop(server);
        for result in [
            client.call(GetInstanceInfo),
            client.call_timeout(GetInstanceInfo, Duration::ZERO),
        ] {
            assert_eq!(
                result.unwrap_err().to_string(),
                "vmm server is no longer running"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_calls_reject_a_stopped_server() {
        let (client, server) = client_server();
        drop(server);
        let error = client.call_async(GetInstanceInfo).await.unwrap_err();
        assert_eq!(error.to_string(), "vmm server is no longer running");
    }

    #[test]
    fn sync_calls_report_a_dropped_reply() {
        for with_timeout in [false, true] {
            let (client, mut server) = client_server();
            let worker = thread::spawn(move || drop(server.0.blocking_recv().unwrap()));
            let error = if with_timeout {
                client.call_timeout(GetInstanceInfo, Duration::from_secs(5))
            } else {
                client.call(GetInstanceInfo)
            }
            .unwrap_err();
            if with_timeout {
                assert_eq!(
                    error.downcast_ref::<oneshot::RecvTimeoutError>(),
                    Some(&oneshot::RecvTimeoutError::Disconnected)
                );
            } else {
                assert!(error.is::<oneshot::RecvError>());
            }
            worker.join().unwrap();
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_calls_report_a_dropped_reply() {
        let (client, mut server) = client_server();
        let worker = tokio::spawn(async move { drop(server.next_request().await.unwrap()) });
        let error = client.call_async(GetInstanceInfo).await.unwrap_err();
        assert!(error.is::<oneshot::RecvError>());
        worker.await.unwrap();
    }

    #[test]
    fn sync_timeout_keeps_the_request_queued_and_tolerates_a_late_reply() {
        for timeout in [Duration::ZERO, Duration::from_millis(10)] {
            let (client, mut server) = client_server();
            let error = client.call_timeout(GetInstanceInfo, timeout).unwrap_err();
            assert_eq!(
                error.downcast_ref::<oneshot::RecvTimeoutError>(),
                Some(&oneshot::RecvTimeoutError::Timeout)
            );
            respond(server.0.try_recv().unwrap());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_async_call_keeps_the_request_queued_and_tolerates_a_late_reply() {
        for timeout in [Duration::ZERO, Duration::from_millis(10)] {
            let (client, mut server) = client_server();
            assert!(
                tokio::time::timeout(timeout, client.call_async(GetInstanceInfo))
                    .await
                    .is_err()
            );
            respond(server.0.try_recv().unwrap());
        }
    }
}
