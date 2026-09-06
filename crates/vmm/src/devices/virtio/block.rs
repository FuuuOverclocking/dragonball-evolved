//! A proof-of-concept virtio-block device.
//!
//! The interesting part is [`BlockDevice::run`]. Compare it with firecracker's
//! block device, which is spread across a `process()` that unpacks a `u32` tag to
//! decide which of its four descriptors woke it, a `register_runtime_events` that
//! rewrites its own epoll registrations on activation, and an
//! `is_io_engine_throttled` flag that exists to remember why it stopped reading a
//! descriptor. All three collapse into the shape of one `async fn`.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::{future, io};

use anyhow::{Context, Result, bail};
use displaydoc::Display;
use logger::{info_unrestricted, warn};
use thiserror::Error;
use tokio::select;
use tokio::time::{self, Instant};
use virtio_queue::desc::split::Descriptor;
use virtio_queue::{DescriptorChain, Queue, QueueOwnedT, QueueT};
use vm_memory::{ByteValued, Bytes, GuestAddress, GuestMemoryError};
use vmm_sys_util::eventfd::EventFd;

use crate::devices::Stop;
use crate::devices::async_fd::AsyncEventFd;
use crate::devices::rate_limiter::{Budget, TokenBucket};
use crate::devices::virtio::{
    Activation, ActivationRx, Activator, GuestMemoryMmap, InterruptKind, IrqTrigger,
    SharedGuestMemory, activation_channel,
};

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_T_GET_ID: u32 = 8;

const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

const SECTOR_SIZE: u64 = 512;
const HEADER_LEN: u32 = 16;
/// Length of the buffer the guest offers for `VIRTIO_BLK_T_GET_ID`.
const ID_LEN: u32 = 20;

/// The header the driver puts in the first, read-only descriptor of every chain.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct RequestHeader {
    request_type: u32,
    _reserved: u32,
    sector: u64,
}

// SAFETY: `RequestHeader` is `repr(C)` plain old data -- 16 bytes of integers with
// no padding and no invalid bit patterns -- so any byte sequence of that length is
// a valid value.
unsafe impl ByteValued for RequestHeader {}

/// Why a single request could not be completed.
///
/// These are per-request faults reported to the driver through the status byte,
/// not device faults: a malformed chain is a guest bug and must not take the vmm
/// down with it.
#[derive(Debug, Display, Error)]
enum RequestError {
    /// descriptor chain is empty
    EmptyChain,
    // `{HEADER_LEN}` would go through displaydoc's `Display` shorthand, which only
    // reaches destructured fields; a constant has to take the `{:?}` path, which
    // prints an integer the same way.
    /// chain does not start with a readable {HEADER_LEN:?}-byte header
    MalformedHeader,
    /// chain does not end with a writable status byte
    MalformedStatus,
    /// payload descriptor has the wrong direction for this request type
    WrongPayloadDirection,
    /// request at sector {sector} of {len} bytes is out of bounds
    OutOfBounds { sector: u64, len: u32 },
    /// write to a read-only disk
    ReadOnly,
    /// unsupported request type {0}
    Unsupported(u32),
    /// guest memory access failed
    Memory(#[from] GuestMemoryError),
    /// disk I/O failed
    Io(#[from] io::Error),
}

/// The backing file, plus the bounds the guest must stay within.
#[derive(Debug)]
struct Disk {
    file: File,
    sectors: u64,
    read_only: bool,
}

impl Disk {
    fn open(path: &Path, read_only: bool) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(!read_only)
            .open(path)
            .with_context(|| format!("open disk image {}", path.display()))?;
        let len = file
            .metadata()
            .with_context(|| format!("stat disk image {}", path.display()))?
            .len();
        Ok(Self {
            file,
            sectors: len / SECTOR_SIZE,
            read_only,
        })
    }

    fn check_bounds(&self, offset: u64, len: u32) -> Result<(), RequestError> {
        let end = offset.checked_add(u64::from(len));
        if end.is_none_or(|end| end > self.sectors * SECTOR_SIZE) {
            return Err(RequestError::OutOfBounds {
                sector: offset / SECTOR_SIZE,
                len,
            });
        }
        Ok(())
    }

    fn read_into(&self, buf: &mut Vec<u8>, offset: u64, len: u32) -> Result<(), RequestError> {
        self.check_bounds(offset, len)?;
        buf.clear();
        buf.resize(len as usize, 0);
        Ok(self.file.read_exact_at(buf, offset)?)
    }

    fn write_from(&self, buf: &[u8], offset: u64) -> Result<(), RequestError> {
        if self.read_only {
            return Err(RequestError::ReadOnly);
        }
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        self.check_bounds(offset, len)?;
        Ok(self.file.write_all_at(buf, offset)?)
    }
}

/// The vcpu-thread-facing half of a block device.
///
/// Everything a vcpu thread needs is here, and it is deliberately small: the
/// device's emulation state stays inside its task, so there is no
/// `Arc<Mutex<dyn ..>>` spanning the two.
#[derive(Debug)]
pub struct BlockHandle {
    /// Used once, when the guest writes `DRIVER_OK`.
    pub activator: Activator,
    /// Registered with `KVM_IOEVENTFD` against the queue notification register.
    pub queue_notifier: EventFd,
    /// Registered with `KVM_IRQFD`.
    pub interrupt: Arc<IrqTrigger>,
}

/// A virtio-block device driven by its own task.
#[derive(Debug)]
pub struct BlockDevice {
    id: String,
    disk: Disk,
    /// Written by KVM when the guest kicks the queue.
    queue_kick: AsyncEventFd,
    activation: ActivationRx,
    /// Requests allowed per window, if the device is capped. The bucket itself is
    /// built once serving starts, so construction needs no runtime context.
    iops: Option<(u64, Duration)>,
    /// Reused across requests so a steady workload stops allocating.
    staging: Vec<u8>,
}

impl BlockDevice {
    /// Open `disk_path` and build a device that is not yet activated.
    ///
    /// `iops` caps the device at that many requests per window.
    pub fn new(
        id: impl Into<String>,
        disk_path: &Path,
        read_only: bool,
        iops: Option<(u64, Duration)>,
    ) -> Result<(Self, BlockHandle)> {
        let id = id.into();
        let disk = Disk::open(disk_path, read_only)?;
        let queue_kick = AsyncEventFd::new().context("create block queue eventfd")?;
        let queue_notifier = queue_kick.notifier().context("clone block queue eventfd")?;
        let interrupt = Arc::new(IrqTrigger::new().context("create block irqfd")?);
        let (activator, activation) = activation_channel();

        let handle = BlockHandle {
            activator,
            queue_notifier,
            interrupt: Arc::clone(&interrupt),
        };
        let device = Self {
            id,
            disk,
            queue_kick,
            activation,
            iops,
            staging: Vec::new(),
        };
        Ok((device, handle))
    }

    /// Drive the device until the vmm stops it.
    pub async fn run(mut self, mut stop: Stop) -> Result<()> {
        // Inactive. Firecracker has to own an `activate_evt` and keep it in epoll
        // just so `process()` gets called and can swap in the runtime
        // registrations; waiting is all that was ever needed.
        let activation = select! {
            biased;
            () = stop.requested() => return Ok(()),
            activation = self.activation.recv() => activation,
        };
        // The guest never brought the device up. That is an ordinary end, not a
        // failure.
        let Some(activation) = activation else {
            return Ok(());
        };

        let mut active = Active::new(activation)
            .with_context(|| format!("activate block device {}", self.id))?;
        info_unrestricted!(
            "block {}: activated, {} sectors, queue size {}",
            self.id,
            self.disk.sectors,
            active.queue.size()
        );

        self.serve(&mut active, &mut stop).await
    }

    async fn serve(&mut self, active: &mut Active, stop: &mut Stop) -> Result<()> {
        let mut limiter = self
            .iops
            .map(|(capacity, window)| TokenBucket::new(capacity, window));

        // `Some` while a rate limit is holding requests back. In firecracker the
        // equivalent state lives in an `is_io_engine_throttled` flag plus a
        // timerfd registration, because a subscriber cannot express "wait for
        // either of these two things" any other way.
        let mut resume_at: Option<Instant> = None;

        loop {
            select! {
                biased;

                () = stop.requested() => return Ok(()),

                // Waiting out a rate limit. Firecracker needs a timerfd, an epoll
                // registration and a `PROCESS_RATE_LIMITER` dispatch arm to get
                // back to this point.
                () = sleep_until_opt(resume_at) => {
                    resume_at = self.drain(active, limiter.as_mut())?;
                }

                // The guest kicked the queue. This one has to stay a descriptor:
                // KVM writes it from the kernel on the mmio exit. While throttled
                // we simply do not read it, and the kernel's eventfd counter holds
                // the pending kicks for us -- no `epoll_ctl` juggling to stop and
                // start listening.
                result = self.queue_kick.read(), if resume_at.is_none() => {
                    result.context("read block queue notification")?;
                    resume_at = self.drain(active, limiter.as_mut())?;
                }
            }
        }
    }

    /// Service the queue until it is empty or a rate limit cuts the drain short.
    ///
    /// Returns when to resume, if throttling stopped it early.
    fn drain(
        &mut self,
        active: &mut Active,
        mut limiter: Option<&mut TokenBucket>,
    ) -> Result<Option<Instant>> {
        let mem = Arc::clone(&active.mem);
        let mut completed_any = false;

        let resume_at = 'drain: loop {
            // Suppress guest kicks for the duration of the drain, then re-enable
            // and re-check, so a chain the driver adds mid-drain cannot be missed.
            active.queue.disable_notification(mem.as_ref())?;

            while let Some(chain) = active.queue.pop_descriptor_chain(Arc::clone(&mem)) {
                if let Some(limiter) = limiter.as_deref_mut()
                    && let Budget::Exhausted { refilled_at } = limiter.consume(1)
                {
                    // Put the chain back. The driver must not see a request the
                    // device has not actually run.
                    active.queue.go_to_previous_position();
                    break 'drain Some(refilled_at);
                }

                let head = chain.head_index();
                let used_len = self.complete(chain, mem.as_ref());
                active.queue.add_used(mem.as_ref(), head, used_len)?;
                completed_any = true;
            }

            if !active.queue.enable_notification(mem.as_ref())? {
                break 'drain None;
            }
        };

        if completed_any && active.queue.needs_notification(mem.as_ref())? {
            active
                .interrupt
                .trigger(InterruptKind::UsedRing)
                .context("raise block used-ring interrupt")?;
        }
        Ok(resume_at)
    }

    /// Run one request and report the used length for its chain.
    fn complete(
        &mut self,
        chain: DescriptorChain<SharedGuestMemory>,
        mem: &GuestMemoryMmap,
    ) -> u32 {
        let mut status_addr = None;
        let (status, data_len) = match self.transfer(chain, mem, &mut status_addr) {
            Ok(data_len) => (VIRTIO_BLK_S_OK, data_len),
            Err(RequestError::Unsupported(request_type)) => {
                warn!("block {}: unsupported request type {request_type}", self.id);
                (VIRTIO_BLK_S_UNSUPP, 0)
            }
            Err(e) => {
                warn!("block {}: request failed: {e}", self.id);
                (VIRTIO_BLK_S_IOERR, 0)
            }
        };

        // Without a writable status descriptor there is nowhere to report the
        // outcome, so the chain is completed with a zero length.
        let Some(status_addr) = status_addr else {
            return 0;
        };
        if let Err(e) = mem.write_obj(status, status_addr) {
            warn!("block {}: cannot write status byte: {e}", self.id);
            return 0;
        }
        // The status byte is part of the chain, so it counts towards used length.
        data_len + 1
    }

    /// Walk the chain, perform the I/O, and report where the status byte goes.
    ///
    /// `status_addr` is an out-parameter, set before any I/O happens, so that the
    /// caller can still report a failure that occurs partway through the payload.
    fn transfer(
        &mut self,
        chain: DescriptorChain<SharedGuestMemory>,
        mem: &GuestMemoryMmap,
        status_addr: &mut Option<GuestAddress>,
    ) -> Result<u32, RequestError> {
        // Find the status descriptor first. It is the last one in the chain, so
        // this costs a second walk, but discovering it only on the way past would
        // leave a mid-payload failure with nowhere to be reported.
        let status_desc = chain.clone().last().ok_or(RequestError::EmptyChain)?;
        if !status_desc.is_write_only() || status_desc.len() < 1 {
            return Err(RequestError::MalformedStatus);
        }
        *status_addr = Some(status_desc.addr());

        let mut descriptors = chain;
        let header_desc = descriptors.next().ok_or(RequestError::EmptyChain)?;
        if header_desc.is_write_only() || header_desc.len() < HEADER_LEN {
            return Err(RequestError::MalformedHeader);
        }
        let header: RequestHeader = mem.read_obj(header_desc.addr())?;

        // Decided up front, so a request type carrying no payload descriptor is
        // still answered rather than silently completed.
        let kind = RequestKind::try_from(header.request_type)?;

        let mut offset =
            header
                .sector
                .checked_mul(SECTOR_SIZE)
                .ok_or(RequestError::OutOfBounds {
                    sector: header.sector,
                    len: 0,
                })?;
        let mut written_to_guest = 0;

        // Everything between the header and the status byte is payload. One
        // descriptor of lookahead spots the status descriptor again without
        // needing its index.
        let mut pending = descriptors.next();
        while let Some(desc) = pending {
            pending = descriptors.next();
            if pending.is_none() {
                break;
            }
            written_to_guest += payload(
                &self.disk,
                &mut self.staging,
                &self.id,
                kind,
                desc,
                mem,
                &mut offset,
            )?;
        }

        if kind == RequestKind::Flush {
            self.disk.file.sync_all()?;
        }
        Ok(written_to_guest)
    }
}

/// The request types this device understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestKind {
    Read,
    Write,
    Flush,
    GetId,
}

impl TryFrom<u32> for RequestKind {
    type Error = RequestError;

    fn try_from(request_type: u32) -> Result<Self, RequestError> {
        match request_type {
            VIRTIO_BLK_T_IN => Ok(Self::Read),
            VIRTIO_BLK_T_OUT => Ok(Self::Write),
            VIRTIO_BLK_T_FLUSH => Ok(Self::Flush),
            VIRTIO_BLK_T_GET_ID => Ok(Self::GetId),
            other => Err(RequestError::Unsupported(other)),
        }
    }
}

/// Move one payload descriptor between the disk and guest memory.
///
/// Free-standing because it needs the disk and the staging buffer borrowed
/// independently.
fn payload(
    disk: &Disk,
    staging: &mut Vec<u8>,
    id: &str,
    kind: RequestKind,
    desc: Descriptor,
    mem: &GuestMemoryMmap,
    offset: &mut u64,
) -> Result<u32, RequestError> {
    let len = desc.len();

    match kind {
        RequestKind::Read => {
            if !desc.is_write_only() {
                return Err(RequestError::WrongPayloadDirection);
            }
            disk.read_into(staging, *offset, len)?;
            mem.write_slice(staging, desc.addr())?;
            *offset += u64::from(len);
            Ok(len)
        }
        RequestKind::Write => {
            if desc.is_write_only() {
                return Err(RequestError::WrongPayloadDirection);
            }
            staging.clear();
            staging.resize(len as usize, 0);
            mem.read_slice(staging, desc.addr())?;
            disk.write_from(staging, *offset)?;
            *offset += u64::from(len);
            Ok(0)
        }
        RequestKind::GetId => {
            if !desc.is_write_only() {
                return Err(RequestError::WrongPayloadDirection);
            }
            let len = len.min(ID_LEN);
            staging.clear();
            staging.resize(len as usize, 0);
            let id = id.as_bytes();
            let copied = id.len().min(staging.len());
            staging[..copied].copy_from_slice(&id[..copied]);
            mem.write_slice(staging, desc.addr())?;
            Ok(len)
        }
        // A flush carries no payload; a driver that supplies one gets it ignored.
        RequestKind::Flush => Ok(0),
    }
}

/// State that only exists once the guest driver has brought the device up.
///
/// Holding it as a separate value is what makes "inactive" unrepresentable in the
/// serving loop: there is no `DeviceState::Inactive` to re-check on every request.
#[derive(Debug)]
struct Active {
    mem: SharedGuestMemory,
    queue: Queue,
    interrupt: Arc<IrqTrigger>,
}

impl Active {
    fn new(activation: Activation) -> Result<Self> {
        let Activation {
            mem,
            queues,
            interrupt,
        } = activation;

        let mut queues = queues.into_iter();
        let queue = queues.next().context("no queue was configured")?;
        if queues.next().is_some() {
            bail!("virtio-block has a single queue");
        }
        if !queue.is_valid(mem.as_ref()) {
            bail!("guest configured an invalid queue");
        }

        Ok(Self {
            mem,
            queue,
            interrupt,
        })
    }
}

/// Sleep until `deadline`, or never if there is none.
///
/// A `select!` arm that must sometimes be inert; the alternative is a guard on
/// every use site.
async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => time::sleep_until(deadline).await,
        None => future::pending().await,
    }
}
