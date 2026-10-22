use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::{future, io};

use anyhow::{Context, Result, bail};
use logger::warn;
use tokio::select;
use tokio::sync::mpsc;
use tokio::time::{self, Instant};
use virtio_queue::desc::split::Descriptor;
use virtio_queue::{DescriptorChain, Queue, QueueOwnedT, QueueState, QueueT};
use vm_memory::{Address, ByteValued, Bytes, GuestMemoryBackend};
use vmm_sys_util::eventfd::{EFD_CLOEXEC, EFD_NONBLOCK, EventFd};

use super::mmio::{DeviceDesc, VERSION_1};
use super::{
    Activation, ActivationRx, Activator, GuestMemoryMmap, InterruptKind, IrqTrigger, Session,
    SharedGuestMemory, activation_channel,
};
use crate::devices::Stop;
use crate::devices::async_fd::AsyncEventFd;
use crate::devices::rate_limiter::{Budget, TokenBucket};

const SECTOR_SIZE: u64 = 512;
const HEADER_LEN: u32 = 16;
const ID_LEN: usize = 20;
const STAGING_SIZE: usize = 64 * 1024;
const BATCH_SIZE: usize = 64;
const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RequestHeader {
    request_type: u32,
    reserved: u32,
    sector: u64,
}

// SAFETY: Integers cover all 16 bytes with no padding or invalid patterns.
unsafe impl ByteValued for RequestHeader {}

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
            .with_context(|| format!("open disk {}", path.display()))?;
        let metadata = file.metadata().context("stat disk")?;
        if !metadata.is_file() {
            bail!("block backing must be a regular raw disk image");
        }
        if metadata.len() < SECTOR_SIZE {
            bail!("disk image must contain at least one sector");
        }
        if !metadata.len().is_multiple_of(SECTOR_SIZE) {
            warn!("disk size is not sector aligned; ignoring trailing bytes");
        }
        Ok(Self {
            file,
            sectors: metadata.len() / SECTOR_SIZE,
            read_only,
        })
    }
}

#[derive(Debug)]
pub struct BlockHandle {
    pub activator: Activator,
    pub queue_notifier: EventFd,
    pub interrupt: Arc<IrqTrigger>,
    pub desc: DeviceDesc,
}

/// Command sent through the block device's own quiesce channel.
enum BlockQuiesceCmd {
    /// Pause I/O and reply with the current queue state.
    Quiesce(oneshot::Sender<Vec<QueueState>>),
    /// Resume I/O from where it left off.
    Resume,
}

/// Handle held outside the device for cooperative quiesce / resume.
///
/// Unlike the gate-based external quiesce, this lets the device export its
/// internal [`QueueState`] at the exact moment it parks.
pub struct BlockQuiesceHandle {
    pub id: String,
    tx: mpsc::Sender<BlockQuiesceCmd>,
}

impl BlockQuiesceHandle {
    /// Ask the block device to quiesce and return its queue states.
    pub async fn quiesce(&self) -> anyhow::Result<Vec<QueueState>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(BlockQuiesceCmd::Quiesce(reply_tx))
            .await
            .map_err(|_| anyhow::anyhow!("block device {} is not running", self.id))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("block device {} dropped quiesce reply", self.id))
    }

    /// Resume the block device after a quiesce.
    pub async fn resume(&self) -> anyhow::Result<()> {
        self.tx
            .send(BlockQuiesceCmd::Resume)
            .await
            .map_err(|_| anyhow::anyhow!("block device {} is not running", self.id))?;
        Ok(())
    }
}

pub struct BlockResources {
    id: String,
    disk: Disk,
    queue_kick: EventFd,
    activation: ActivationRx,
    iops: Option<(u64, Duration)>,
    quiesce_rx: mpsc::Receiver<BlockQuiesceCmd>,
}

impl BlockResources {
    pub fn open(
        id: impl Into<String>,
        path: &Path,
        read_only: bool,
        iops: Option<(u64, Duration)>,
    ) -> Result<(Self, BlockHandle, BlockQuiesceHandle)> {
        let id = id.into();
        let disk = Disk::open(path, read_only)?;
        if let Some((capacity, window)) = iops
            && (capacity == 0 || window.is_zero())
        {
            bail!("IOPS capacity and window must be non-zero");
        }
        let queue_kick = EventFd::new(EFD_NONBLOCK | EFD_CLOEXEC)?;
        let queue_notifier = queue_kick.try_clone()?;
        let interrupt = Arc::new(IrqTrigger::new()?);
        let (activator, activation) = activation_channel();
        let (quiesce_tx, quiesce_rx) = mpsc::channel(1);
        let desc = DeviceDesc {
            device_type: 2,
            features: VERSION_1 | (1 << 9) | if read_only { 1 << 5 } else { 0 },
            queue_max: vec![256],
            config: disk.sectors.to_le_bytes().to_vec(),
        };
        let handle = BlockHandle {
            activator,
            queue_notifier,
            interrupt,
            desc,
        };
        let quiesce_handle = BlockQuiesceHandle {
            id: id.clone(),
            tx: quiesce_tx,
        };
        Ok((
            Self {
                id,
                disk,
                queue_kick,
                activation,
                iops,
                quiesce_rx,
            },
            handle,
            quiesce_handle,
        ))
    }

    pub fn into_device(self) -> Result<BlockDevice> {
        Ok(BlockDevice {
            id: self.id,
            disk: self.disk,
            queue_kick: AsyncEventFd::from_event_fd(self.queue_kick)?,
            activation: self.activation,
            iops: self.iops,
            staging: Vec::new(),
            quiesce_rx: self.quiesce_rx,
        })
    }
}

pub struct BlockDevice {
    id: String,
    disk: Disk,
    queue_kick: AsyncEventFd,
    activation: ActivationRx,
    iops: Option<(u64, Duration)>,
    staging: Vec<u8>,
    quiesce_rx: mpsc::Receiver<BlockQuiesceCmd>,
}

impl BlockDevice {
    pub fn new(
        id: impl Into<String>,
        path: &Path,
        read_only: bool,
        iops: Option<(u64, Duration)>,
    ) -> Result<(Self, BlockHandle, BlockQuiesceHandle)> {
        let (resources, handle, quiesce_handle) = BlockResources::open(id, path, read_only, iops)?;
        Ok((resources.into_device()?, handle, quiesce_handle))
    }

    pub async fn run(mut self, mut stop: Stop) -> Result<()> {
        loop {
            // Wait for activation, handling quiesce while inactive.
            let activation = loop {
                select! {
                    biased;
                    () = stop.requested() => return Ok(()),
                    Some(cmd) = self.quiesce_rx.recv() => {
                        if let BlockQuiesceCmd::Quiesce(reply) = cmd {
                            let _ = reply.send(Vec::new());
                            // Park until resume or stop.
                            loop {
                                select! {
                                    biased;
                                    () = stop.requested() => return Ok(()),
                                    cmd = self.quiesce_rx.recv() => match cmd {
                                        Some(BlockQuiesceCmd::Resume) => break,
                                        Some(BlockQuiesceCmd::Quiesce(reply)) => {
                                            let _ = reply.send(Vec::new());
                                        }
                                        None => return Ok(()),
                                    },
                                }
                            }
                        }
                    }
                    activation = self.activation.recv() => break activation,
                }
            };
            let Some(Activation {
                mem,
                queues,
                interrupt,
                driver_features: _,
                mut session,
            }) = activation
            else {
                return Ok(());
            };
            let mut queues = queues.into_iter();
            let queue = queues.next().context("block queue was not configured")?;
            if queues.next().is_some() || !queue.is_valid(mem.as_ref()) {
                bail!("invalid block queue configuration");
            }
            let mut active = Active {
                mem,
                queue,
                interrupt,
            };
            let result = self.serve(&mut active, &mut stop, &mut session).await;
            drop(active);
            drop(session);
            result?;
        }
    }

    async fn serve(
        &mut self,
        active: &mut Active,
        stop: &mut Stop,
        session: &mut Session,
    ) -> Result<()> {
        let mut limiter = self
            .iops
            .map(|(capacity, window)| TokenBucket::new(capacity, window));
        let mut resume_at = Some(Instant::now());
        loop {
            select! {
                biased;
                () = stop.requested() => return Ok(()),
                () = session.requested() => return Ok(()),
                Some(cmd) = self.quiesce_rx.recv() => match cmd {
                    BlockQuiesceCmd::Quiesce(reply) => {
                        let _ = reply.send(vec![active.queue.state()]);
                        // Park until resume or stop.
                        loop {
                            select! {
                                biased;
                                () = stop.requested() => return Ok(()),
                                cmd = self.quiesce_rx.recv() => match cmd {
                                    Some(BlockQuiesceCmd::Resume) => break,
                                    Some(BlockQuiesceCmd::Quiesce(reply)) => {
                                        let _ = reply.send(vec![active.queue.state()]);
                                    }
                                    None => return Ok(()),
                                },
                            }
                        }
                    }
                    BlockQuiesceCmd::Resume => {}
                },
                () = sleep_until_opt(resume_at) => resume_at = self.drain(active, limiter.as_mut())?,
                result = self.queue_kick.read(), if resume_at.is_none() => {
                    result.context("read block queue notification")?;
                    resume_at = self.drain(active, limiter.as_mut())?;
                }
            }
            tokio::task::yield_now().await;
        }
    }

    fn drain(
        &mut self,
        active: &mut Active,
        mut limiter: Option<&mut TokenBucket>,
    ) -> Result<Option<Instant>> {
        let mem = Arc::clone(&active.mem);
        let mut completed = 0;
        let resume_at = 'drain: loop {
            let available = active
                .queue
                .avail_idx(mem.as_ref(), std::sync::atomic::Ordering::Acquire)?
                .0;
            if available.wrapping_sub(active.queue.next_avail()) > active.queue.size() {
                bail!("virtio available index exceeds queue capacity");
            }
            active.queue.disable_notification(mem.as_ref())?;
            while let Some(chain) = active.queue.pop_descriptor_chain(Arc::clone(&mem)) {
                if let Some(limiter) = limiter.as_deref_mut()
                    && let Budget::Exhausted { refilled_at } = limiter.consume(1)
                {
                    active.queue.go_to_previous_position();
                    break 'drain Some(refilled_at);
                }
                let head = chain.head_index();
                let used_len = self.complete(chain, mem.as_ref());
                active.queue.add_used(mem.as_ref(), head, used_len)?;
                completed += 1;
                if completed == BATCH_SIZE {
                    break 'drain Some(Instant::now());
                }
            }
            if !active.queue.enable_notification(mem.as_ref())? {
                break None;
            }
        };
        if completed > 0 && active.queue.needs_notification(mem.as_ref())? {
            active.interrupt.trigger(InterruptKind::UsedRing)?;
        }
        Ok(resume_at)
    }

    fn complete(
        &mut self,
        chain: DescriptorChain<SharedGuestMemory>,
        mem: &GuestMemoryMmap,
    ) -> u32 {
        let descriptors: Vec<_> = chain.collect();
        let Some(status) = descriptors
            .last()
            .filter(|d| d.is_write_only() && d.len() >= 1 && !d.has_next())
        else {
            return 0;
        };
        if !mem.check_range(status.addr(), 1) {
            return 0;
        }
        let (status_byte, data_len) = match self.transfer(&descriptors, mem) {
            Ok(len) => (S_OK, len),
            Err(TransferError::Unsupported) => (S_UNSUPP, 0),
            Err(TransferError::Io(error)) => {
                warn!("block {} I/O failed: {error}", self.id);
                (S_IOERR, 0)
            }
            Err(TransferError::Invalid) => {
                warn!("block {} rejected an invalid request", self.id);
                (S_IOERR, 0)
            }
        };
        if mem.write_obj(status_byte, status.addr()).is_err() {
            return 0;
        }
        data_len + 1
    }

    fn transfer(
        &mut self,
        descriptors: &[Descriptor],
        mem: &GuestMemoryMmap,
    ) -> Result<u32, TransferError> {
        if descriptors.len() < 2 {
            return Err(TransferError::Invalid);
        }
        let header = descriptors[0];
        if header.is_write_only() || header.len() < HEADER_LEN {
            return Err(TransferError::Invalid);
        }
        let header: RequestHeader = mem
            .read_obj(header.addr())
            .map_err(|_| TransferError::Invalid)?;
        let kind = match header.request_type {
            0 => Kind::Read,
            1 => Kind::Write,
            4 => Kind::Flush,
            8 => Kind::GetId,
            _ => return Err(TransferError::Unsupported),
        };
        let payload = &descriptors[1..descriptors.len() - 1];
        let mut total = 0u64;
        for desc in payload {
            if (kind == Kind::Write && desc.is_write_only())
                || (matches!(kind, Kind::Read | Kind::GetId) && !desc.is_write_only())
                || !mem.check_range(desc.addr(), desc.len() as usize)
            {
                return Err(TransferError::Invalid);
            }
            total = total
                .checked_add(u64::from(desc.len()))
                .ok_or(TransferError::Invalid)?;
        }
        let mut offset = if matches!(kind, Kind::Read | Kind::Write) {
            header
                .sector
                .checked_mul(SECTOR_SIZE)
                .ok_or(TransferError::Invalid)?
        } else {
            0
        };
        if matches!(kind, Kind::Read | Kind::Write)
            && (total == 0
                || !total.is_multiple_of(SECTOR_SIZE)
                || offset
                    .checked_add(total)
                    .is_none_or(|end| end > self.disk.sectors * SECTOR_SIZE)
                || (kind == Kind::Write && self.disk.read_only))
        {
            return Err(TransferError::Invalid);
        }
        if kind == Kind::Flush {
            self.disk.file.sync_all()?;
            return Ok(0);
        }
        let mut written = 0;
        for desc in payload {
            let len = desc.len() as usize;
            match kind {
                Kind::Read | Kind::Write => {
                    let mut transferred = 0;
                    while transferred < len {
                        let count = (len - transferred).min(STAGING_SIZE);
                        self.staging.resize(count, 0);
                        let address = desc
                            .addr()
                            .checked_add(transferred as u64)
                            .ok_or(TransferError::Invalid)?;
                        if kind == Kind::Read {
                            self.disk.file.read_exact_at(&mut self.staging, offset)?;
                            mem.write_slice(&self.staging, address)
                                .map_err(|_| TransferError::Invalid)?;
                            written += count as u32;
                        } else {
                            mem.read_slice(&mut self.staging, address)
                                .map_err(|_| TransferError::Invalid)?;
                            self.disk.file.write_all_at(&self.staging, offset)?;
                        }
                        offset += count as u64;
                        transferred += count;
                    }
                }
                Kind::GetId => {
                    let remaining = ID_LEN.saturating_sub(written as usize);
                    let len = len.min(remaining);
                    let mut id = [0u8; ID_LEN];
                    let copied = self.id.len().min(ID_LEN);
                    id[..copied].copy_from_slice(&self.id.as_bytes()[..copied]);
                    mem.write_slice(&id[written as usize..written as usize + len], desc.addr())
                        .map_err(|_| TransferError::Invalid)?;
                    written += len as u32;
                }
                Kind::Flush => unreachable!(),
            }
        }
        Ok(written)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Read,
    Write,
    Flush,
    GetId,
}

enum TransferError {
    Invalid,
    Unsupported,
    Io(io::Error),
}
impl From<io::Error> for TransferError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

struct Active {
    mem: SharedGuestMemory,
    queue: Queue,
    interrupt: Arc<IrqTrigger>,
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => time::sleep_until(deadline).await,
        None => future::pending().await,
    }
}
