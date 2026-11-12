use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use api::{RawVmmOp, VmmRequest};
use tokio::sync::mpsc;

use crate::VmmExitStatus;

/// The vmm thread's end of the request channel.
pub(crate) struct VmmServer {
    rx: mpsc::UnboundedReceiver<VmmRequest>,
}

impl VmmServer {
    pub(crate) fn new(rx: mpsc::UnboundedReceiver<VmmRequest>) -> Self {
        Self { rx }
    }
}

/// Cloneable VMM request sender; clones share a single-use join handle and keep the request channel open.
#[derive(Clone)]
pub struct VmmClient {
    pub(crate) thread_handle: Arc<Mutex<Option<thread::JoinHandle<VmmExitStatus>>>>,
    pub(crate) tx: mpsc::UnboundedSender<VmmRequest>,
}

impl VmmClient {
    pub async fn join_vmm_thread_async(&self) -> Result<VmmExitStatus> {
        let handle = self
            .thread_handle
            .lock()
            .map_err(|e| anyhow!("lock vmm thread handle: {e}"))?
            .take()
            .context("vmm thread handle was already taken")?;

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

    pub fn call_drop_response<Req: RawVmmOp>(&self, request: Req) -> Result<()> {
        let (op, _) = request.into_vmm_request();
        self.send(op)
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
