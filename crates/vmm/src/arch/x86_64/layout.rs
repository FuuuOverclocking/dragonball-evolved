use std::range::{Range, RangeInclusive};

use thiserror::Error;

use crate::resources::{AddressPool, GpaRange, Gsi, IdPool, ResourceBook};

pub const GDT_START: u64 = 0x500;
pub const IDT_START: u64 = 0x520;
pub const PVH_INFO_START: u64 = 0x6000;
pub const MODLIST_START: u64 = 0x6040;
pub const MEMMAP_START: u64 = 0x7000;
pub const CMDLINE_START: u64 = 0x2_0000;
pub const CMDLINE_MAX_SIZE: usize = 2048;
pub const SYSTEM_MEM_START: u64 = 0x9_fc00;
pub const SYSTEM_MEM_END: u64 = 0xe_0000;
pub const HIMEM_START: u64 = 0x10_0000;
pub const MMIO32_START: u64 = 0xc000_0000;
pub const MMIO32_END: u64 = 0x1_0000_0000;
pub const IOAPIC_START: u64 = 0xfec0_0000;
pub const LAPIC_START: u64 = 0xfee0_0000;
pub const TSS_START: u64 = 0xfffb_d000;
// Bounded: never reaches the IOAPIC, LAPIC or TSS, and leaves the rest of the hole free.
pub const VIRTIO_MMIO_WINDOW: GpaRange = GpaRange::new(MMIO32_START, MMIO32_START + (16 << 20));
const _: () = assert!(VIRTIO_MMIO_WINDOW.end() <= IOAPIC_START);
pub const VIRTIO_MMIO_LEN: u64 = 0x1000;
pub const LEGACY_GSIS: RangeInclusive<Gsi> = RangeInclusive {
    start: Gsi(5),
    last: Gsi(23),
};
pub const I8042_PORTS: Range<u64> = Range {
    start: 0x60,
    end: 0x65,
};
pub const COM1_PORTS: Range<u64> = Range {
    start: 0x3f8,
    end: 0x400,
};
pub const COM1_GSI: Gsi = Gsi(4);
pub const PAGE_SIZE: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryRegion {
    pub start: u64,
    pub len: u64,
}

impl MemoryRegion {
    pub fn end(self) -> u64 {
        self.start + self.len
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LayoutError {
    #[error("guest memory size must be non-zero")]
    Empty,
    #[error("guest memory layout overflows the physical address space")]
    Overflow,
}

pub fn ram_regions(size: u64) -> Result<Vec<MemoryRegion>, LayoutError> {
    if size == 0 {
        return Err(LayoutError::Empty);
    }
    if size <= MMIO32_START {
        return Ok(vec![MemoryRegion {
            start: 0,
            len: size,
        }]);
    }

    let high_len = size - MMIO32_START;
    MMIO32_END
        .checked_add(high_len)
        .ok_or(LayoutError::Overflow)?;
    Ok(vec![
        MemoryRegion {
            start: 0,
            len: MMIO32_START,
        },
        MemoryRegion {
            start: MMIO32_END,
            len: high_len,
        },
    ])
}

pub fn resource_book() -> ResourceBook {
    ResourceBook {
        virtio_mmio: AddressPool::new("virtio-mmio", VIRTIO_MMIO_WINDOW),
        legacy_gsi: IdPool::new("legacy GSI", LEGACY_GSIS),
    }
}

pub fn align_down(value: u64, alignment: u64) -> u64 {
    debug_assert!(alignment.is_power_of_two());
    value & !(alignment - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_stays_contiguous_below_the_mmio_hole() {
        assert_eq!(
            ram_regions(128 << 20).unwrap(),
            [MemoryRegion {
                start: 0,
                len: 128 << 20,
            }]
        );
    }

    #[test]
    fn ram_continues_above_four_gib() {
        assert_eq!(
            ram_regions(4 << 30).unwrap(),
            [
                MemoryRegion {
                    start: 0,
                    len: 3 << 30,
                },
                MemoryRegion {
                    start: 4 << 30,
                    len: 1 << 30,
                },
            ]
        );
    }

    #[test]
    fn invalid_sizes_are_rejected() {
        assert_eq!(ram_regions(0), Err(LayoutError::Empty));
        assert_eq!(ram_regions(u64::MAX), Err(LayoutError::Overflow));
    }
}
