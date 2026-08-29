//! The vmm's request surface.
//!
//! This enum is expected to grow to dozens of variants, so its shape matters more
//! than it looks. Two decisions do most of the work:
//!
//! **Each variant carries its own reply channel.** The alternative -- one flat
//! `VmmResponse` enum shared by every request -- means a caller asking for one
//! thing must still match on every response the vmm could ever produce and write
//! `unreachable!()` for the rest. Firecracker lives with exactly that: `VmmData`
//! has a variant per query, and `_ => unreachable!()` appears wherever a response
//! is consumed. Here `client.request(VmmRequest::Status)` is typed to return a
//! [`VmmStatus`], and nothing else can come back.
//!
//! **Failure is per-request, not global.** There is no crate-wide error enum. A
//! request that can fail says so in its own reply type -- `Reply<Result<T,
//! ThatRequestsError>>` -- and requests that cannot fail do not force their
//! callers to handle errors that will never arrive. Firecracker funnels everything
//! through one `VmmActionError`, which every caller then has to destructure.
//!
//! The cost is that a request is no longer plain data: it owns a channel, so it is
//! neither `Clone` nor serializable. That is deliberate. Wire formats, and
//! anything reflected over for a TUI, GUI or HTTP schema, belong in the parameter
//! types a variant carries, not in the plumbing that delivers them.

use std::fmt;
use std::path::PathBuf;

/// A request the vmm thread will answer.
pub enum VmmRequest {
    /// Stop the vmm. Answered once every device has quiesced, so receiving the
    /// reply means shutdown finished rather than shutdown started.
    Shutdown(Reply<()>),
    /// What the vmm is currently doing.
    Status(Reply<VmmStatus>),
    /// Run the deliberately small KVM proof of concept.
    StartPoc(PocConfig, Reply<Result<PocReport, PocError>>),
}

impl VmmRequest {
    /// A stable name for logs and metrics.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Shutdown(_) => "Shutdown",
            Self::Status(_) => "Status",
            Self::StartPoc(_, _) => "StartPoc",
        }
    }
}

impl fmt::Debug for VmmRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A snapshot of what the vmm is doing, for a monitor or a status view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmmStatus {
    pub devices_running: usize,
    pub vcpu_running: bool,
    pub memory_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PocConfig {
    pub disk_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PocReport {
    pub used_index: u16,
    pub request_status: u8,
    pub data: Vec<u8>,
    pub interrupt_status: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PocError {
    pub message: String,
}

impl PocError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for PocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PocError {}

/// The channel a request is answered on.
///
/// `Send`, so a handler that would take a while can move this into a task and
/// answer later rather than making the vmm's main loop wait for it.
pub struct Reply<T>(oneshot::Sender<T>);

impl<T> Reply<T> {
    /// Answer the request.
    ///
    /// A caller that stopped waiting is not the vmm's problem, so a dropped
    /// receiver is ignored rather than reported.
    pub fn send(self, value: T) {
        let _ = self.0.send(value);
    }
}

impl<T> fmt::Debug for Reply<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Reply")
    }
}

/// The receiving half of a reply channel.
///
/// Supports blocking, timed and awaited receives, which is what lets one request
/// enum serve both synchronous embedders and async ones.
pub type Answer<T> = oneshot::Receiver<T>;

/// Create the two halves of a request's reply channel.
pub fn reply_channel<T>() -> (Reply<T>, Answer<T>) {
    let (tx, rx) = oneshot::channel();
    (Reply(tx), rx)
}
