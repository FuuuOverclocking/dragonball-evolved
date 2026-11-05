use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use api::{RawVmmOp, VmmRequest};
use tokio::sync::mpsc;

/// The vmm thread's end of the request channel.
pub(crate) struct VmmServer {
    /// `None` once `Detached` has been reported.
    rx: Option<mpsc::UnboundedReceiver<VmmRequest>>,
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