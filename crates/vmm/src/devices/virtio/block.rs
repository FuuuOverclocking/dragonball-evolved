use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::{future, io};

use tokio::time::{self, Instant};
use virtio_queue::desc::split::Descriptor;
use virtio_queue::{DescriptorChain, Queue, QueueOwnedT, QueueState, QueueT};
use vm_memory::{Address, ByteValued, Bytes, GuestMemoryBackend, GuestMemoryMmap};
use vmm_sys_util::eventfd::{EFD_CLOEXEC, EFD_NONBLOCK, EventFd};

use super::mmio::{DeviceDesc, VERSION_1};
use super::{
    Activation, ActivationRx, Activator, InterruptKind, IrqTrigger, Session, activation_channel,
};
use crate::devices::async_fd::AsyncEventFd;
use crate::devices::control::{Command, Control};
use crate::memory::store::MemoryView;

const SECTOR_SIZE: u64 = 512;
const HEADER_LEN: u32 = 16;
const ID_LEN: usize = 20;
const QUEUE_MAX: u16 = 256;
const STAGING_SIZE: usize = 64 * 1024;
const BATCH_SIZE: usize = 64;
const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum BlockError {
    /// open the block backing file
    Open(#[source] io::Error),
    /// stat the block backing file
    Metadata(#[source] io::Error),
    /// block backing must be a regular raw disk image
    NotFile,
    /// disk image must contain at least one sector
    TooSmall,
    /// IOPS capacity and window must be non-zero and representable
    InvalidIops,
    /// create or register the block notification descriptors
    Notification(#[source] io::Error),
    /// invalid block queue configuration
    Configuration,
    /// virtio available index exceeds queue capacity
    AvailableIndex,
    /// access the block virtqueue
    Queue(#[from] virtio_queue::Error),
    /// inject the block interrupt
    Interrupt(#[source] io::Error),
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RequestHeader {
    request_type: u32,
    reserved: u32,
    sector: u64,
}

// SAFETY: integers cover all 16 bytes without padding or invalid patterns.
unsafe impl ByteValued for RequestHeader {}

struct Disk {
    file: File,
    sectors: u64,
    read_only: bool,
}

impl Disk {
    fn open(path: &Path, read_only: bool) -> Result<Self, BlockError> {
        let file = OpenOptions::new()
            .read(true)
            .write(!read_only)
            .open(path)
            .map_err(BlockError::Open)?;
        let metadata = file.metadata().map_err(BlockError::Metadata)?;
        if !metadata.is_file() {
            return Err(BlockError::NotFile);
        }
        if metadata.len() < SECTOR_SIZE {
            return Err(BlockError::TooSmall);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockQuiesced {
    Inactive,
    Active(QueueState),
}

pub struct BlockResources {
    id: [u8; ID_LEN],
    disk: Disk,
    queue_kick: EventFd,
    activation: ActivationRx,
    iops: Option<(u64, Duration)>,
}

impl BlockResources {
    pub fn open(
        id: &str,
        path: &Path,
        read_only: bool,
        iops: Option<(u64, Duration)>,
    ) -> Result<(Self, BlockHandle), BlockError> {
        let disk = Disk::open(path, read_only)?;
        if let Some((capacity, window)) = iops
            && (capacity == 0 || window.is_zero() || Instant::now().checked_add(window).is_none())
        {
            return Err(BlockError::InvalidIops);
        }
        let queue_kick =
            EventFd::new(EFD_NONBLOCK | EFD_CLOEXEC).map_err(BlockError::Notification)?;
        let queue_notifier = queue_kick.try_clone().map_err(BlockError::Notification)?;
        let interrupt = Arc::new(IrqTrigger::new().map_err(BlockError::Notification)?);
        let (activator, activation) = activation_channel();
        let desc = DeviceDesc {
            device_type: 2,
            features: VERSION_1 | (1 << 9) | if read_only { 1 << 5 } else { 0 },
            queue_max: vec![QUEUE_MAX],
            config: disk.sectors.to_le_bytes().to_vec(),
        };
        let mut disk_id = [0; ID_LEN];
        let copied = id.len().min(ID_LEN);
        disk_id[..copied].copy_from_slice(&id.as_bytes()[..copied]);
        Ok((
            Self {
                id: disk_id,
                disk,
                queue_kick,
                activation,
                iops,
            },
            BlockHandle {
                activator,
                queue_notifier,
                interrupt,
                desc,
            },
        ))
    }

    pub fn into_device(self) -> Result<BlockDevice, BlockError> {
        Ok(BlockDevice {
            id: self.id,
            disk: self.disk,
            queue_kick: AsyncEventFd::from_event_fd(self.queue_kick)
                .map_err(BlockError::Notification)?,
            activation: self.activation,
            iops: self.iops,
            staging: vec![0; STAGING_SIZE],
        })
    }
}

pub struct BlockDevice {
    id: [u8; ID_LEN],
    disk: Disk,
    queue_kick: AsyncEventFd,
    activation: ActivationRx,
    iops: Option<(u64, Duration)>,
    staging: Vec<u8>,
}

impl BlockDevice {
    pub async fn run(mut self, mut control: Control<BlockQuiesced>) -> Result<(), BlockError> {
        let mut active: Option<Active> = None;
        let mut parked = false;
        let mut resume_at = None;
        let mut limiter = None;
        loop {
            tokio::select! {
                biased;
                command = control.recv() => match command {
                    Command::Stop => {
                        release(&mut active);
                        return Ok(());
                    }
                    Command::Quiesce(ack) => {
                        if active.is_none() && let Some(activation) = self.activation.try_recv() {
                            active = Some(Active::new(activation)?);
                            limiter = self.iops.map(|(capacity, window)| Limiter::new(capacity, window));
                            resume_at = Some(Instant::now());
                        }
                        parked = true;
                        ack.send(quiesced(&active));
                    }
                    Command::Resume(ack) => {
                        parked = false;
                        ack.send(());
                    }
                },
                () = reset_requested(&mut active) => {
                    release(&mut active);
                    limiter = None;
                    resume_at = None;
                }
                activation = self.activation.recv(), if active.is_none() => {
                    let Some(activation) = activation else { return Ok(()); };
                    active = Some(Active::new(activation)?);
                    limiter = self.iops.map(|(capacity, window)| Limiter::new(capacity, window));
                    resume_at = Some(Instant::now());
                }
                () = sleep_until(resume_at), if !parked && active.is_some() => {
                    resume_at = self.drain(active.as_mut().unwrap(), limiter.as_mut())?;
                }
                result = self.queue_kick.read(), if !parked && active.is_some() && resume_at.is_none() => {
                    result.map_err(BlockError::Notification)?;
                    resume_at = self.drain(active.as_mut().unwrap(), limiter.as_mut())?;
                }
            }
            tokio::task::yield_now().await;
        }
    }

    fn drain(
        &mut self,
        active: &mut Active,
        mut limiter: Option<&mut Limiter>,
    ) -> Result<Option<Instant>, BlockError> {
        let mem = Arc::clone(&active.mem);
        let mut completed = 0;
        let resume_at = 'drain: loop {
            let completed_before = completed;
            active.queue.disable_notification(mem.as_ref())?;
            loop {
                let available = active.queue.avail_idx(mem.as_ref(), Ordering::Acquire)?.0;
                if available.wrapping_sub(active.queue.next_avail()) > active.queue.size() {
                    return Err(BlockError::AvailableIndex);
                }
                let Some(chain) = active.queue.iter(Arc::clone(&mem))?.next() else {
                    if available != active.queue.next_avail() {
                        return Err(BlockError::Queue(virtio_queue::Error::InvalidChain));
                    }
                    break;
                };
                if let Some(limiter) = limiter.as_deref_mut()
                    && let Some(deadline) = limiter.consume()
                {
                    active.queue.go_to_previous_position();
                    break 'drain Some(deadline);
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
            // New work may race with an empty drain; retry cooperatively instead of spinning.
            if completed == completed_before {
                break Some(Instant::now());
            }
        };
        if completed > 0 && active.queue.needs_notification(mem.as_ref())? {
            active
                .interrupt
                .trigger(InterruptKind::UsedRing)
                .map_err(BlockError::Interrupt)?;
        }
        Ok(resume_at)
    }

    fn complete(&mut self, chain: DescriptorChain<MemoryView>, mem: &GuestMemoryMmap<()>) -> u32 {
        let mut descriptors = [Descriptor::default(); QUEUE_MAX as usize];
        let mut count = 0;
        for descriptor in chain {
            let Some(slot) = descriptors.get_mut(count) else {
                return 0;
            };
            *slot = descriptor;
            count += 1;
        }
        let descriptors = &descriptors[..count];
        let Some(status) = descriptors
            .last()
            .filter(|d| d.is_write_only() && d.len() >= 1 && !d.has_next())
        else {
            return 0;
        };
        if !mem.check_range(status.addr(), 1) {
            return 0;
        }
        let (status_byte, data_len) = match self.transfer(descriptors, mem) {
            Ok(len) => (S_OK, len),
            Err(TransferError::Unsupported) => (S_UNSUPP, 0),
            Err(TransferError::Io(_) | TransferError::Invalid) => (S_IOERR, 0),
        };
        if mem.write_obj(status_byte, status.addr()).is_err() {
            return 0;
        }
        data_len + 1
    }

    fn transfer(
        &mut self,
        descriptors: &[Descriptor],
        mem: &GuestMemoryMmap<()>,
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
        let kind = match u32::from_le(header.request_type) {
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
            u64::from_le(header.sector)
                .checked_mul(SECTOR_SIZE)
                .ok_or(TransferError::Invalid)?
        } else {
            0
        };
        if matches!(kind, Kind::Read | Kind::Write)
            && (total == 0
                || !total.is_multiple_of(SECTOR_SIZE)
                || total >= u64::from(u32::MAX)
                || offset
                    .checked_add(total)
                    .is_none_or(|end| end > self.disk.sectors * SECTOR_SIZE)
                || (kind == Kind::Write && self.disk.read_only))
        {
            return Err(TransferError::Invalid);
        }
        if kind == Kind::Flush {
            if !payload.is_empty() {
                return Err(TransferError::Invalid);
            }
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
                        let staging = &mut self.staging[..count];
                        let address = desc
                            .addr()
                            .checked_add(transferred as u64)
                            .ok_or(TransferError::Invalid)?;
                        if kind == Kind::Read {
                            self.disk.file.read_exact_at(staging, offset)?;
                            mem.write_slice(staging, address)
                                .map_err(|_| TransferError::Invalid)?;
                            written += count as u32;
                        } else {
                            mem.read_slice(staging, address)
                                .map_err(|_| TransferError::Invalid)?;
                            self.disk.file.write_all_at(staging, offset)?;
                        }
                        offset += count as u64;
                        transferred += count;
                    }
                }
                Kind::GetId => {
                    let len = len.min(ID_LEN.saturating_sub(written as usize));
                    mem.write_slice(
                        &self.id[written as usize..written as usize + len],
                        desc.addr(),
                    )
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

#[derive(Debug, displaydoc::Display, thiserror::Error)]
enum TransferError {
    /// invalid block request
    Invalid,
    /// unsupported block request
    Unsupported,
    /// block backing I/O failed
    Io(#[from] io::Error),
}

struct Active {
    queue: Queue,
    mem: MemoryView,
    interrupt: Arc<IrqTrigger>,
    // Field order also releases queue and RAM before the session during unwinding.
    session: Session,
}

impl Active {
    fn new(mut activation: Activation) -> Result<Self, BlockError> {
        if activation.queues.len() != 1 {
            return Err(BlockError::Configuration);
        }
        let queue = &activation.queues[0];
        if queue.size() > QUEUE_MAX
            || !queue.is_valid(activation.mem.as_ref())
            || activation.driver_features & VERSION_1 == 0
        {
            return Err(BlockError::Configuration);
        }
        Ok(Self {
            queue: activation.queues.pop().unwrap(),
            mem: activation.mem,
            interrupt: activation.interrupt,
            session: activation.session,
        })
    }
}

fn release(active: &mut Option<Active>) {
    if let Some(Active {
        queue,
        mem,
        interrupt,
        session,
    }) = active.take()
    {
        drop((queue, mem, interrupt));
        drop(session);
    }
}

fn quiesced(active: &Option<Active>) -> BlockQuiesced {
    active.as_ref().map_or(BlockQuiesced::Inactive, |active| {
        BlockQuiesced::Active(active.queue.state())
    })
}

async fn reset_requested(active: &mut Option<Active>) {
    match active {
        Some(active) => active.session.requested().await,
        None => future::pending().await,
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => time::sleep_until(deadline).await,
        None => future::pending().await,
    }
}

struct Limiter {
    capacity: u64,
    available: u64,
    fractional: u128,
    window: Duration,
    last_refill: Instant,
}

impl Limiter {
    fn new(capacity: u64, window: Duration) -> Self {
        Self {
            capacity,
            available: capacity,
            fractional: 0,
            window,
            last_refill: Instant::now(),
        }
    }

    fn consume(&mut self) -> Option<Instant> {
        self.consume_at(Instant::now())
    }

    fn consume_at(&mut self, now: Instant) -> Option<Instant> {
        let credit = now
            .duration_since(self.last_refill)
            .as_nanos()
            .saturating_mul(u128::from(self.capacity))
            .saturating_add(self.fractional);
        let gained = credit / self.window.as_nanos();
        self.available = self.capacity.min(
            self.available
                .saturating_add(gained.min(u128::from(u64::MAX)) as u64),
        );
        // Keep sub-token credit across refills, but never bank it beyond a full bucket.
        self.fractional = if self.available == self.capacity {
            0
        } else {
            credit % self.window.as_nanos()
        };
        self.last_refill = now;
        if self.available > 0 {
            self.available -= 1;
            None
        } else {
            let nanos =
                (self.window.as_nanos() - self.fractional).div_ceil(u128::from(self.capacity));
            Some(
                now + Duration::new(
                    (nanos / 1_000_000_000) as u64,
                    (nanos % 1_000_000_000) as u32,
                ),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use vm_memory::GuestAddress;

    use super::*;
    use crate::devices::virtio::session_channel;

    #[test]
    fn limiter_preserves_fractional_refill_credit() {
        let mut limiter = Limiter::new(2, Duration::from_millis(100));
        let start = limiter.last_refill;
        assert_eq!(limiter.consume_at(start), None);
        assert_eq!(limiter.consume_at(start), None);
        assert_eq!(limiter.consume_at(start + Duration::from_millis(75)), None);
        assert_eq!(limiter.consume_at(start + Duration::from_millis(100)), None);
        assert_eq!(
            limiter.consume_at(start + Duration::from_millis(100)),
            Some(start + Duration::from_millis(150))
        );
    }

    #[test]
    fn limiter_keeps_credit_when_polled_before_a_whole_token() {
        let mut limiter = Limiter::new(2, Duration::from_millis(100));
        let start = limiter.last_refill;
        assert_eq!(limiter.consume_at(start), None);
        assert_eq!(limiter.consume_at(start), None);
        for millis in [25, 49] {
            assert_eq!(
                limiter.consume_at(start + Duration::from_millis(millis)),
                Some(start + Duration::from_millis(50))
            );
        }
        assert_eq!(limiter.consume_at(start + Duration::from_millis(50)), None);
    }

    #[test]
    fn limiter_never_banks_credit_beyond_capacity() {
        let mut limiter = Limiter::new(2, Duration::from_millis(100));
        let start = limiter.last_refill + Duration::from_millis(175);
        assert_eq!(limiter.consume_at(start), None);
        assert_eq!(limiter.consume_at(start), None);
        assert_eq!(
            limiter.consume_at(start),
            Some(start + Duration::from_millis(50))
        );
    }

    #[tokio::test(flavor = "local")]
    async fn one_drain_completes_exactly_one_bounded_batch() {
        const REQUESTS: u16 = 70;
        let mem = Arc::new(GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap());
        let mut queue = Queue::new(QUEUE_MAX).unwrap();
        queue.set_size(QUEUE_MAX);
        queue.set_desc_table_address(Some(0x1000), Some(0));
        queue.set_avail_ring_address(Some(0x2000), Some(0));
        queue.set_used_ring_address(Some(0x3000), Some(0));
        queue.set_ready(true);
        for i in 0..REQUESTS {
            let header = 0x4000 + u64::from(i) * 32;
            mem.write_obj(
                RequestHeader {
                    request_type: 8u32.to_le(),
                    ..RequestHeader::default()
                },
                GuestAddress(header),
            )
            .unwrap();
            mem.write_obj(0xffu8, GuestAddress(header + 16)).unwrap();
            mem.write_obj(
                Descriptor::new(header, HEADER_LEN, 1, i * 2 + 1),
                GuestAddress(0x1000 + u64::from(i) * 32),
            )
            .unwrap();
            mem.write_obj(
                Descriptor::new(header + 16, 1, 2, 0),
                GuestAddress(0x1010 + u64::from(i) * 32),
            )
            .unwrap();
            mem.write_obj((i * 2).to_le(), GuestAddress(0x2004 + u64::from(i) * 2))
                .unwrap();
        }
        mem.write_obj(REQUESTS.to_le(), GuestAddress(0x2002))
            .unwrap();
        let (resources, handle) =
            BlockResources::open("batch", &std::env::current_exe().unwrap(), true, None).unwrap();
        let mut device = resources.into_device().unwrap();
        let (_session, session) = session_channel();
        let mut active = Active::new(Activation {
            mem,
            queues: vec![queue],
            interrupt: handle.interrupt,
            driver_features: VERSION_1,
            session,
        })
        .unwrap();

        let resume_at = device.drain(&mut active, None).unwrap();
        assert_eq!(BATCH_SIZE, 64);
        assert_eq!(active.queue.next_avail(), 64);
        assert_eq!(active.queue.next_used(), 64);
        assert!(resume_at.is_some());
        assert_eq!(
            active
                .queue
                .used_idx(active.mem.as_ref(), Ordering::Acquire)
                .unwrap()
                .0,
            64
        );
        assert_eq!(
            active
                .mem
                .read_obj::<u8>(GuestAddress(0x4000 + 63 * 32 + 16))
                .unwrap(),
            S_OK
        );
        assert_eq!(
            active
                .mem
                .read_obj::<u8>(GuestAddress(0x4000 + 64 * 32 + 16))
                .unwrap(),
            0xff
        );
        assert_eq!(device.drain(&mut active, None).unwrap(), None);
        assert_eq!(active.queue.next_avail(), REQUESTS);
        assert_eq!(active.queue.next_used(), REQUESTS);
        assert_eq!(
            active
                .mem
                .read_obj::<u8>(GuestAddress(0x4000 + 69 * 32 + 16))
                .unwrap(),
            S_OK
        );
    }
}
