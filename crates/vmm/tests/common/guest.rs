use std::sync::Arc;
use std::sync::atomic::Ordering;

use virtio_queue::{Queue, QueueT};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryMmap};
use vmm::memory::store::MemoryView;

pub const QUEUE_SIZE: u16 = 16;
pub const SECTOR_SIZE: usize = 512;
pub const DESC_TABLE: u64 = 0x1000;
pub const AVAIL_RING: u64 = 0x2000;
pub const USED_RING: u64 = 0x3000;
const MEM_SIZE: usize = 4 * 1024 * 1024;

pub const VIRTIO_BLK_T_IN: u32 = 0;
pub const VIRTIO_BLK_T_OUT: u32 = 1;
pub const VIRTIO_BLK_T_FLUSH: u32 = 4;
pub const VIRTIO_BLK_T_GET_ID: u32 = 8;
pub const VIRTIO_BLK_S_OK: u8 = 0;
pub const VIRTIO_BLK_S_IOERR: u8 = 1;
pub const VIRTIO_BLK_S_UNSUPP: u8 = 2;

#[derive(Debug, Clone, Copy)]
pub struct Segment {
    pub addr: GuestAddress,
    pub len: u32,
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

pub struct GuestDriver {
    pub mem: MemoryView,
    size: u16,
    next_desc: u16,
    next_avail: u16,
    next_buffer: u64,
}

impl GuestDriver {
    pub fn new() -> Self {
        Self::with_queue_size(QUEUE_SIZE)
    }

    pub fn with_queue_size(size: u16) -> Self {
        let mem = Arc::new(GuestMemoryMmap::from_ranges(&[(GuestAddress(0), MEM_SIZE)]).unwrap());
        Self {
            mem,
            size,
            next_desc: 0,
            next_avail: 0,
            next_buffer: 0x10000,
        }
    }

    pub fn reset_queue(&mut self) {
        self.mem
            .write_slice(&[0; 0x2900], GuestAddress(DESC_TABLE))
            .unwrap();
        self.next_desc = 0;
        self.next_avail = 0;
        self.next_buffer = 0x10000;
    }

    pub fn queue(&self) -> Queue {
        let mut queue = Queue::new(self.size).unwrap();
        queue.set_size(self.size);
        queue.set_desc_table_address(Some(DESC_TABLE as u32), Some(0));
        queue.set_avail_ring_address(Some(AVAIL_RING as u32), Some(0));
        queue.set_used_ring_address(Some(USED_RING as u32), Some(0));
        queue.set_ready(true);
        queue
    }

    pub fn alloc(&mut self, len: u32) -> GuestAddress {
        let addr = GuestAddress(self.next_buffer);
        self.next_buffer += u64::from(len).next_multiple_of(16);
        addr
    }

    pub fn request_header(&mut self, request_type: u32, sector: u64) -> GuestAddress {
        let addr = self.alloc(16);
        self.mem.write_obj(request_type.to_le(), addr).unwrap();
        self.mem.write_obj(0u32, addr.unchecked_add(4)).unwrap();
        self.mem
            .write_obj(sector.to_le(), addr.unchecked_add(8))
            .unwrap();
        addr
    }

    pub fn submit(&mut self, segments: &[Segment]) -> u16 {
        assert!(!segments.is_empty());
        assert!(usize::from(self.next_desc) + segments.len() <= usize::from(self.size));
        let head = self.next_desc;
        for (i, segment) in segments.iter().enumerate() {
            let index = head + i as u16;
            let flags =
                u16::from(i + 1 != segments.len()) | (u16::from(segment.device_writable) << 1);
            let at = DESC_TABLE + u64::from(index) * 16;
            self.mem
                .write_obj(segment.addr.0.to_le(), GuestAddress(at))
                .unwrap();
            self.mem
                .write_obj(segment.len.to_le(), GuestAddress(at + 8))
                .unwrap();
            self.mem
                .write_obj(flags.to_le(), GuestAddress(at + 12))
                .unwrap();
            self.mem
                .write_obj((index + 1).to_le(), GuestAddress(at + 14))
                .unwrap();
        }
        self.next_desc += segments.len() as u16;
        let slot = u64::from(self.next_avail % self.size);
        self.mem
            .write_obj(head.to_le(), GuestAddress(AVAIL_RING + 4 + slot * 2))
            .unwrap();
        self.next_avail = self.next_avail.wrapping_add(1);
        self.mem
            .store(
                self.next_avail.to_le(),
                GuestAddress(AVAIL_RING + 2),
                Ordering::Release,
            )
            .unwrap();
        head
    }

    pub fn used_idx(&self) -> u16 {
        u16::from_le(
            self.mem
                .load(GuestAddress(USED_RING + 2), Ordering::Acquire)
                .unwrap(),
        )
    }

    pub fn used_entry(&self, slot: u16) -> (u32, u32) {
        let at = USED_RING + 4 + u64::from(slot % self.size) * 8;
        (
            u32::from_le(self.mem.read_obj(GuestAddress(at)).unwrap()),
            u32::from_le(self.mem.read_obj(GuestAddress(at + 4)).unwrap()),
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
