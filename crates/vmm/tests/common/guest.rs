//! A stand-in for the guest driver.
//!
//! There is no KVM in these tests, so the test itself plays the two roles KVM
//! would: it lays out a split virtqueue in memory the device can reach, and it
//! writes the ioeventfd that the kernel would write when the guest kicks the
//! queue notification register. Everything the device does in between is real.

#![allow(dead_code)]

use std::sync::Arc;

use virtio_queue::{Queue, QueueT};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryMmap};
use vmm::devices::virtio::SharedGuestMemory;

pub const QUEUE_SIZE: u16 = 16;
pub const SECTOR_SIZE: usize = 512;

const MEM_SIZE: usize = 0x40000;
const DESC_TABLE: u64 = 0x1000;
const AVAIL_RING: u64 = 0x2000;
const USED_RING: u64 = 0x3000;
const BUFFER_AREA: u64 = 0x10000;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

pub const VIRTIO_BLK_T_IN: u32 = 0;
pub const VIRTIO_BLK_T_OUT: u32 = 1;
pub const VIRTIO_BLK_T_FLUSH: u32 = 4;
pub const VIRTIO_BLK_T_GET_ID: u32 = 8;

pub const VIRTIO_BLK_S_OK: u8 = 0;
pub const VIRTIO_BLK_S_IOERR: u8 = 1;
pub const VIRTIO_BLK_S_UNSUPP: u8 = 2;

/// One descriptor of a chain the driver is about to publish.
#[derive(Debug, Clone, Copy)]
pub struct Segment {
    pub addr: GuestAddress,
    pub len: u32,
    /// Set when the device is expected to write into the buffer.
    pub device_writable: bool,
}

impl Segment {
    pub fn readable(addr: GuestAddress, len: u32) -> Self {
        Self {
            addr,
            len,
            device_writable: false,
        }
    }

    pub fn writable(addr: GuestAddress, len: u32) -> Self {
        Self {
            addr,
            len,
            device_writable: true,
        }
    }
}

/// Guest memory plus the bookkeeping a driver would keep.
///
/// All multi-byte fields are written host-endian, which matches virtio's
/// little-endian layout on the targets this runs on. It is the same assumption
/// the device makes when it reads its request headers.
pub struct GuestDriver {
    pub mem: SharedGuestMemory,
    next_desc: u16,
    next_avail: u16,
    next_buffer: u64,
}

impl GuestDriver {
    pub fn new() -> Self {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), MEM_SIZE)])
            .expect("map fake guest memory");
        Self {
            mem: Arc::new(mem),
            next_desc: 0,
            next_avail: 0,
            next_buffer: BUFFER_AREA,
        }
    }

    /// A queue configured exactly as a driver would leave it after negotiation.
    pub fn queue(&self) -> Queue {
        let mut queue = Queue::new(QUEUE_SIZE).expect("build queue");
        queue.set_size(QUEUE_SIZE);
        queue.set_desc_table_address(Some(DESC_TABLE as u32), Some(0));
        queue.set_avail_ring_address(Some(AVAIL_RING as u32), Some(0));
        queue.set_used_ring_address(Some(USED_RING as u32), Some(0));
        queue.set_ready(true);
        queue
    }

    /// Carve out a buffer for the device to read or write.
    pub fn alloc(&mut self, len: u32) -> GuestAddress {
        let addr = GuestAddress(self.next_buffer);
        // Keep buffers apart so an overrun shows up as a failed assertion rather
        // than as silent corruption of the next one.
        self.next_buffer += u64::from(len).next_multiple_of(16);
        addr
    }

    /// Write a virtio-block request header into a freshly allocated descriptor.
    pub fn request_header(&mut self, request_type: u32, sector: u64) -> GuestAddress {
        let addr = self.alloc(16);
        self.mem.write_obj(request_type, addr).unwrap();
        self.mem.write_obj(0u32, addr.unchecked_add(4)).unwrap();
        self.mem.write_obj(sector, addr.unchecked_add(8)).unwrap();
        addr
    }

    /// Publish a descriptor chain and return its head index.
    pub fn submit(&mut self, segments: &[Segment]) -> u16 {
        assert!(
            !segments.is_empty(),
            "a chain needs at least one descriptor"
        );
        let head = self.next_desc;

        for (i, segment) in segments.iter().enumerate() {
            let index = head + i as u16;
            let is_last = i + 1 == segments.len();
            let mut flags = 0;
            if segment.device_writable {
                flags |= VIRTQ_DESC_F_WRITE;
            }
            if !is_last {
                flags |= VIRTQ_DESC_F_NEXT;
            }
            self.write_desc(index, segment, flags, index + 1);
        }
        self.next_desc += segments.len() as u16;

        // Publish the head, then make it visible by bumping `avail.idx`.
        let slot = u64::from(self.next_avail % QUEUE_SIZE);
        self.mem
            .write_obj(head, GuestAddress(AVAIL_RING + 4 + slot * 2))
            .unwrap();
        self.next_avail = self.next_avail.wrapping_add(1);
        self.mem
            .write_obj(self.next_avail, GuestAddress(AVAIL_RING + 2))
            .unwrap();

        head
    }

    fn write_desc(&self, index: u16, segment: &Segment, flags: u16, next: u16) {
        let at = DESC_TABLE + u64::from(index) * 16;
        self.mem
            .write_obj(segment.addr.0, GuestAddress(at))
            .unwrap();
        self.mem
            .write_obj(segment.len, GuestAddress(at + 8))
            .unwrap();
        self.mem.write_obj(flags, GuestAddress(at + 12)).unwrap();
        self.mem.write_obj(next, GuestAddress(at + 14)).unwrap();
    }

    /// How many chains the device has completed.
    pub fn used_idx(&self) -> u16 {
        self.mem.read_obj(GuestAddress(USED_RING + 2)).unwrap()
    }

    /// The `(descriptor index, bytes written)` pair the device published.
    pub fn used_entry(&self, slot: u16) -> (u32, u32) {
        let at = USED_RING + 4 + u64::from(slot) * 8;
        (
            self.mem.read_obj(GuestAddress(at)).unwrap(),
            self.mem.read_obj(GuestAddress(at + 4)).unwrap(),
        )
    }

    pub fn read_bytes(&self, addr: GuestAddress, len: usize) -> Vec<u8> {
        let mut buf = vec![0; len];
        self.mem.read_slice(&mut buf, addr).unwrap();
        buf
    }

    pub fn write_bytes(&self, addr: GuestAddress, bytes: &[u8]) {
        self.mem.write_slice(bytes, addr).unwrap();
    }
}
