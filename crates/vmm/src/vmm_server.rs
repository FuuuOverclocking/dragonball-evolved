use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use api::{RawVmmOp, VmmRequest};
use tokio::sync::mpsc;

use crate::VmmExitStatus;

/// What the control plane delivered to the vmm thread.
pub(crate) enum Inbound {
    Request(VmmRequest),
    /// Every client has gone away; reported once.
    Detached,
}

/// The vmm thread's end of the request channel.
pub(crate) struct VmmServer {
    /// `None` once `Detached` has been reported.
    rx: Option<mpsc::UnboundedReceiver<VmmRequest>>,
}

impl VmmServer {
    pub(crate) fn new(rx: mpsc::UnboundedReceiver<VmmRequest>) -> Self {
        Self { rx: Some(rx) }
    }

    /// Wait for the next request, or `Detached` once every client has gone away.
    ///
    /// `Detached` is reported exactly once; later calls never resolve, because a
    /// closed channel would otherwise turn every `select!` round into a spin.
    /// Cancel-safe, so it can sit directly in a `select!` arm.
    pub(crate) async fn next_inbound(&mut self) -> Inbound {
        let Some(rx) = self.rx.as_mut() else {
            return std::future::pending().await;
        };
        match rx.recv().await {
            Some(request) => Inbound::Request(request),
            None => {
                self.rx = None;
                Inbound::Detached
            }
        }
    }
}

/// vmm server is no longer running
#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub struct ServerStopped;

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum CallError {
    /// send the request
    Send(#[from] ServerStopped),
    /// vmm dropped the request without replying
    NoReply,
    /// timed out waiting for the vmm reply
    TimedOut,
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum JoinError {
    /// vmm thread was already joined
    AlreadyJoined,
    /// vmm thread panicked
    Panicked,
    /// wait for the vmm thread on the blocking pool
    BlockingPool(#[source] tokio::task::JoinError),
}

/// Cloneable VMM request sender; clones share a single-use join handle and keep the request channel open.
#[derive(Clone)]
pub struct VmmClient {
    thread: Arc<Mutex<Option<JoinHandle<VmmExitStatus>>>>,
    tx: mpsc::UnboundedSender<VmmRequest>,
}

impl VmmClient {
    pub(crate) fn new(
        tx: mpsc::UnboundedSender<VmmRequest>,
        thread: Option<JoinHandle<VmmExitStatus>>,
    ) -> Self {
        Self {
            thread: Arc::new(Mutex::new(thread)),
            tx,
        }
    }

    fn take_thread(&self) -> Result<JoinHandle<VmmExitStatus>, JoinError> {
        // Nothing can panic while the lock is held, so a poisoned lock is still consistent.
        self.thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .ok_or(JoinError::AlreadyJoined)
    }

    /// Blocks for exit without requesting shutdown; only the first join gets the status.
    pub fn join_vmm_thread(&self) -> Result<VmmExitStatus, JoinError> {
        self.take_thread()?.join().map_err(|_| JoinError::Panicked)
    }

    /// Like [`Self::join_vmm_thread`] via Tokio's blocking pool; once started, cancellation forfeits the result.
    pub async fn async_join_vmm_thread(&self) -> Result<VmmExitStatus, JoinError> {
        let thread = self.take_thread()?;
        tokio::task::spawn_blocking(move || thread.join())
            .await
            .map_err(JoinError::BlockingPool)?
            .map_err(|_| JoinError::Panicked)
    }

    /// Hand a request that already carries its own reply channel to the vmm thread.
    ///
    /// Success says the request was queued and nothing more: whether the operation
    /// ran, and what it produced, is still only what its reply says. An error means
    /// the vmm is gone, so the request it took with it will never be answered.
    pub fn send(&self, request: VmmRequest) -> Result<(), ServerStopped> {
        self.tx.send(request).map_err(|_| ServerStopped)
    }

    /// Blocks for a typed reply; channel failures are outer errors, while operation results stay in the response.
    pub fn call<Req: RawVmmOp>(&self, request: Req) -> Result<Req::Response, CallError> {
        let (request, reply) = request.into_vmm_request();
        self.send(request)?;
        reply.recv().map_err(|_| CallError::NoReply)
    }

    /// Like [`Self::call`], but limits reply waiting after enqueueing; timeout does not cancel the operation.
    pub fn call_timeout<Req: RawVmmOp>(
        &self,
        request: Req,
        timeout: Duration,
    ) -> Result<Req::Response, CallError> {
        let (request, reply) = request.into_vmm_request();
        self.send(request)?;
        reply.recv_timeout(timeout).map_err(|error| match error {
            oneshot::RecvTimeoutError::Timeout => CallError::TimedOut,
            oneshot::RecvTimeoutError::Disconnected => CallError::NoReply,
        })
    }

    /// Like [`Self::call`] without blocking; dropping its future after enqueueing does not cancel the operation.
    pub async fn call_async<Req: RawVmmOp>(
        &self,
        request: Req,
    ) -> Result<Req::Response, CallError> {
        let (request, reply) = request.into_vmm_request();
        self.send(request)?;
        reply.await.map_err(|_| CallError::NoReply)
    }
}

#[cfg(test)]
pub(crate) fn pair() -> (VmmClient, VmmServer) {
    let (tx, rx) = mpsc::unbounded_channel();
    (VmmClient::new(tx, None), VmmServer::new(rx))
}

#[cfg(test)]
mod tests {
    use std::thread;

    use api::block::{AddDisk, BlockDevConfig};
    use api::instance_info::{GetInstanceInfo, InstanceInfo, VmState};

    use super::*;

    async fn next_request(server: &mut VmmServer) -> VmmRequest {
        match server.next_inbound().await {
            Inbound::Request(request) => request,
            Inbound::Detached => panic!("every client detached"),
        }
    }

    fn instance_info() -> InstanceInfo {
        InstanceInfo {
            id: "test-vm".into(),
            version: "1.0".into(),
            state: VmState::Running,
        }
    }

    fn add_disk() -> AddDisk {
        AddDisk(BlockDevConfig {
            id: None,
            path: "/test/disk".into(),
            read_only: false,
            root: false,
        })
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
            let (client, mut server) = pair();
            let worker = thread::spawn(move || {
                let rx = server.rx.as_mut().unwrap();
                respond(rx.blocking_recv().unwrap());
                respond(rx.blocking_recv().unwrap());
            });
            let info = if with_timeout {
                client.call_timeout(GetInstanceInfo, Duration::from_secs(5))
            } else {
                client.call(GetInstanceInfo)
            };
            assert_eq!(info.unwrap(), instance_info());
            let result = if with_timeout {
                client.call_timeout(add_disk(), Duration::from_secs(5))
            } else {
                client.call(add_disk())
            };
            assert_eq!(result.unwrap(), Err(api::ApiError::msg("disk failed")));
            worker.join().unwrap();
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_calls_preserve_responses_and_operation_errors() {
        let (client, mut server) = pair();
        let worker = tokio::spawn(async move {
            respond(next_request(&mut server).await);
            respond(next_request(&mut server).await);
        });
        assert_eq!(
            client.call_async(GetInstanceInfo).await.unwrap(),
            instance_info()
        );
        assert_eq!(
            client.call_async(add_disk()).await.unwrap(),
            Err(api::ApiError::msg("disk failed"))
        );
        worker.await.unwrap();
    }

    #[test]
    fn sync_calls_reject_a_stopped_server() {
        let (client, server) = pair();
        drop(server);
        let (request, _reply) = GetInstanceInfo.into_vmm_request();
        assert_eq!(client.send(request), Err(ServerStopped));
        for result in [
            client.call(GetInstanceInfo),
            client.call_timeout(GetInstanceInfo, Duration::ZERO),
        ] {
            assert_eq!(result.unwrap_err(), CallError::Send(ServerStopped));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_calls_reject_a_stopped_server() {
        let (client, server) = pair();
        drop(server);
        let error = client.call_async(GetInstanceInfo).await.unwrap_err();
        assert_eq!(error, CallError::Send(ServerStopped));
    }

    #[test]
    fn sync_calls_report_a_dropped_reply() {
        for with_timeout in [false, true] {
            let (client, mut server) = pair();
            let worker =
                thread::spawn(move || drop(server.rx.as_mut().unwrap().blocking_recv().unwrap()));
            let error = if with_timeout {
                client.call_timeout(GetInstanceInfo, Duration::from_secs(5))
            } else {
                client.call(GetInstanceInfo)
            }
            .unwrap_err();
            assert_eq!(error, CallError::NoReply);
            worker.join().unwrap();
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_calls_report_a_dropped_reply() {
        let (client, mut server) = pair();
        let worker = tokio::spawn(async move { drop(next_request(&mut server).await) });
        let error = client.call_async(GetInstanceInfo).await.unwrap_err();
        assert_eq!(error, CallError::NoReply);
        worker.await.unwrap();
    }

    #[test]
    fn sync_timeout_keeps_the_request_queued_and_tolerates_a_late_reply() {
        for timeout in [Duration::ZERO, Duration::from_millis(10)] {
            let (client, mut server) = pair();
            let error = client.call_timeout(GetInstanceInfo, timeout).unwrap_err();
            assert_eq!(error, CallError::TimedOut);
            respond(server.rx.as_mut().unwrap().try_recv().unwrap());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_async_call_keeps_the_request_queued_and_tolerates_a_late_reply() {
        for timeout in [Duration::ZERO, Duration::from_millis(10)] {
            let (client, mut server) = pair();
            assert!(
                tokio::time::timeout(timeout, client.call_async(GetInstanceInfo))
                    .await
                    .is_err()
            );
            respond(server.rx.as_mut().unwrap().try_recv().unwrap());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn losing_every_client_detaches_once_then_stays_pending() {
        let (client, mut server) = pair();
        let clone = client.clone();
        drop(client);
        drop(clone);
        assert!(matches!(server.next_inbound().await, Inbound::Detached));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), server.next_inbound())
                .await
                .is_err()
        );
    }

    #[test]
    fn only_the_first_join_gets_the_exit_status() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = VmmClient::new(tx, Some(thread::spawn(|| VmmExitStatus::Ok)));
        let clone = client.clone();
        assert_eq!(client.join_vmm_thread().unwrap(), VmmExitStatus::Ok);
        assert!(matches!(
            clone.join_vmm_thread(),
            Err(JoinError::AlreadyJoined)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_join_reports_the_exit_status_or_a_panic() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let status = VmmExitStatus::Error("failed".into());
        let returned = status.clone();
        let client = VmmClient::new(tx.clone(), Some(thread::spawn(move || returned)));
        assert_eq!(client.async_join_vmm_thread().await.unwrap(), status);
        assert!(matches!(
            client.async_join_vmm_thread().await,
            Err(JoinError::AlreadyJoined)
        ));

        let client = VmmClient::new(tx, Some(thread::spawn(|| panic!("vmm thread failed"))));
        assert!(matches!(
            client.async_join_vmm_thread().await,
            Err(JoinError::Panicked)
        ));
    }
}
