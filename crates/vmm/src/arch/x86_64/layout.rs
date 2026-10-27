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
pub const DEVICE_MEMORY_ALIGN: u64 = 1 << 30;
pub const DEVICE_MEMORY_MAX_SIZE: u64 = 1 << 40;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LayoutError {
    #[error("guest memory size must be non-zero")]
    Empty,
    #[error("guest memory layout overflows the physical address space")]
    Overflow,
}

/// Lays `size` bytes of RAM out around the 32-bit MMIO hole.
pub fn ram_regions(size: u64) -> Result<Vec<GpaRange>, LayoutError> {
    if size == 0 {
        return Err(LayoutError::Empty);
    }
    if size <= MMIO32_START {
        return Ok(vec![GpaRange::new(0, size)]);
    }
    let high_end = MMIO32_END
        .checked_add(size - MMIO32_START)
        .ok_or(LayoutError::Overflow)?;
    Ok(vec![
        GpaRange::new(0, MMIO32_START),
        GpaRange::new(MMIO32_END, high_end),
    ])
}

pub fn device_window(ram_end: u64, physical_limit: u64) -> Result<GpaRange, LayoutError> {
    let start = ram_end
        .max(MMIO32_END)
        .checked_next_multiple_of(DEVICE_MEMORY_ALIGN)
        .ok_or(LayoutError::Overflow)?;
    let size = physical_limit
        .checked_sub(start)
        .ok_or(LayoutError::Overflow)?
        .min(DEVICE_MEMORY_MAX_SIZE);
    Ok(GpaRange::new(start, start + size))
}

pub fn resource_book(device_window: GpaRange) -> ResourceBook {
    ResourceBook {
        virtio_mmio: AddressPool::new("virtio-mmio", VIRTIO_MMIO_WINDOW),
        legacy_gsi: IdPool::new("legacy GSI", LEGACY_GSIS),
        device_memory: AddressPool::new("device memory", device_window),
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
            [GpaRange::new(0, 128 << 20)]
        );
    }

    #[test]
    fn ram_continues_above_four_gib() {
        assert_eq!(
            ram_regions(4 << 30).unwrap(),
            [GpaRange::new(0, 3 << 30), GpaRange::new(4 << 30, 5 << 30)]
        );
    }

    #[test]
    fn invalid_sizes_are_rejected() {
        assert_eq!(ram_regions(0), Err(LayoutError::Empty));
        assert_eq!(ram_regions(u64::MAX), Err(LayoutError::Overflow));
    }

    #[test]
    fn device_memory_starts_after_ram_and_the_mmio_hole() {
        for (size, start) in [
            (128 << 20, 4 << 30),
            (3 << 30, 4 << 30),
            ((3 << 30) + (1 << 20), 5 << 30),
            (4 << 30, 5 << 30),
        ] {
            let ram = ram_regions(size).unwrap();
            let window = device_window(ram.last().unwrap().end(), 1 << 46).unwrap();
            assert_eq!(window, GpaRange::new(start, start + (1 << 40)));
            assert!(ram.iter().all(|range| !range.overlaps(&window)));
        }
    }

    #[test]
    fn device_memory_is_clipped_to_the_physical_address_limit() {
        assert_eq!(
            device_window(63 << 30, 1 << 36).unwrap(),
            GpaRange::new(63 << 30, 64 << 30)
        );
        let empty = device_window(128 << 20, 1 << 32).unwrap();
        assert_eq!(empty, GpaRange::new(4 << 30, 4 << 30));
        assert!(
            resource_book(empty)
                .device_memory
                .allocate(1 << 30, 1 << 30)
                .is_err()
        );
        assert_eq!(device_window(65 << 30, 1 << 36), Err(LayoutError::Overflow));
        assert_eq!(
            device_window(u64::MAX, u64::MAX),
            Err(LayoutError::Overflow)
        );
    }
}
