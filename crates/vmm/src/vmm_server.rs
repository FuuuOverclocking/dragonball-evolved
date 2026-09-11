use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use api::{Answer, Reply, VmmRequest};
use tokio::sync::mpsc;

use crate::{InstanceInfo, VmmExitStatus};

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

#[derive(Clone)]
pub struct VmmClient {
    pub(crate) thread_handle: Arc<Mutex<Option<thread::JoinHandle<VmmExitStatus>>>>,
    pub(crate) instance_info: Arc<InstanceInfo>,
    pub(crate) tx: mpsc::UnboundedSender<VmmRequest>,
}

impl VmmClient {
    pub fn join_vmm_thread(&self) -> Result<VmmExitStatus> {
        let handle = self.take_thread_handle()?;
        handle.join().map_err(|_| anyhow!("vmm thread panicked"))
    }

    pub async fn async_join_vmm_thread(&self) -> Result<VmmExitStatus> {
        let handle = self.take_thread_handle()?;
        tokio::task::spawn_blocking(move || {
            handle.join().map_err(|_| anyhow!("vmm thread panicked"))
        })
        .await
        .context("join tokio blocking task")?
    }

    pub fn instance_info(&self) -> &InstanceInfo {
        &self.instance_info
    }

    /// Send a request and block until the vmm answers.
    ///
    /// `request` is the variant's own constructor, which is what ties the return
    /// type to the request: `client.request(VmmRequest::Status)` yields a
    /// `VmmStatus`. Variants that take parameters are passed as a closure, as in
    /// `client.request(|reply| VmmRequest::Something(config, reply))`.
    ///
    /// These three methods stay generic rather than growing a typed wrapper per
    /// request, so a request enum with dozens of variants does not turn into
    /// dozens of near-identical client methods.
    pub fn request<T>(&self, request: impl FnOnce(Reply<T>) -> VmmRequest) -> Result<T> {
        self.send(request)?
            .recv()
            .context("vmm dropped the reply channel")
    }

    pub fn request_timeout<T>(
        &self,
        request: impl FnOnce(Reply<T>) -> VmmRequest,
        timeout: Duration,
    ) -> Result<T> {
        self.send(request)?
            .recv_timeout(timeout)
            .context("recv vmm reply")
    }

    pub async fn request_async<T>(
        &self,
        request: impl FnOnce(Reply<T>) -> VmmRequest,
    ) -> Result<T> {
        self.send(request)?
            .await
            .context("vmm dropped the reply channel")
    }

    /// Hand a request to the vmm thread and return the channel its reply arrives
    /// on.
    ///
    /// Safe to call from any thread, and it neither blocks nor needs a runtime.
    fn send<T>(&self, request: impl FnOnce(Reply<T>) -> VmmRequest) -> Result<Answer<T>> {
        let (reply, answer) = api::reply_channel();
        self.tx
            .send(request(reply))
            .map_err(|_| anyhow!("vmm server is no longer running"))?;
        Ok(answer)
    }

    fn take_thread_handle(&self) -> Result<thread::JoinHandle<VmmExitStatus>> {
        self.thread_handle
            .lock()
            .map_err(|e| anyhow!("lock vmm thread handle: {e}"))?
            .take()
            .context("vmm thread handle was already taken")
    }
}
