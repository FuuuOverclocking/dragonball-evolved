use crate::align::{Alignment, GuestPageSize};
use crate::space::{Gpa, Gsi, Pio, Range, Raw, Space};

pub const GDT_START: Gpa = Gpa(0x500);
pub const IDT_START: Gpa = Gpa(0x520);
pub const PVH_INFO_START: Gpa = Gpa(0x6000);
pub const MODLIST_START: Gpa = Gpa(0x6040);
pub const MEMMAP_START: Gpa = Gpa(0x7000);
pub const CMDLINE_START: Gpa = Gpa(0x2_0000);
pub const CMDLINE_MAX_SIZE: usize = 2048;
/// The system data area: reserved in the memory map, holds the MP table.
pub const SYSTEM_MEM: Range<Gpa> =
    Range::<Gpa>::const_new(Gpa(0x9_fc00), 0xe_0000 - 0x9_fc00).unwrap();
pub const HIMEM_START: Gpa = Gpa(0x10_0000);
pub const MMIO32_START: Gpa = Gpa(0xc000_0000);
pub const MMIO32_END: Gpa = Gpa(0x1_0000_0000);
pub const IOAPIC_START: Gpa = Gpa(0xfec0_0000);
pub const LAPIC_START: Gpa = Gpa(0xfee0_0000);
pub const TSS_START: Gpa = Gpa(0xfffb_d000);
// Bounded: never reaches the IOAPIC, LAPIC or TSS, and leaves the rest of the hole free.
pub const VIRTIO_MMIO_WINDOW: Range<Gpa> = Range::<Gpa>::const_new(MMIO32_START, 16 << 20).unwrap();
const _: () = assert!(VIRTIO_MMIO_WINDOW.start().0 + VIRTIO_MMIO_WINDOW.len() <= IOAPIC_START.0);
pub const VIRTIO_MMIO_LEN: u64 = 0x1000;
pub const LEGACY_GSIS: Range<Gsi> = Range::<Gsi>::const_inclusive(Gsi(5), Gsi(23)).unwrap();
pub const I8042_PORTS: Range<Pio> = Range::<Pio>::const_new(Pio(0x60), 5).unwrap();
pub const COM1_PORTS: Range<Pio> = Range::<Pio>::const_new(Pio(0x3f8), 8).unwrap();
pub const COM1_GSI: Gsi = Gsi(4);
pub const DEVICE_MEMORY_ALIGN: Alignment = Alignment::new(1 << 30).unwrap();
pub const DEVICE_MEMORY_MAX_SIZE: u64 = 1 << 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum LayoutError {
    /// guest memory size must be non-zero
    Empty,
    /// guest memory size {0:#x} is not a multiple of the guest page
    Unaligned(u64),
    /// guest memory layout overflows the physical address space
    Overflow,
}

/// Lays `size` bytes of RAM out around the 32-bit MMIO hole.
pub fn ram_regions(size: u64) -> Result<Vec<Range<Gpa>>, LayoutError> {
    if size == 0 {
        return Err(LayoutError::Empty);
    }
    if !size.is_aligned(GuestPageSize::X86_4K.alignment()) {
        return Err(LayoutError::Unaligned(size));
    }
    let region = |start, len| Range::new(start, len).ok_or(LayoutError::Overflow);
    match size.checked_sub(MMIO32_START.0) {
        Some(high) if high > 0 => Ok(vec![
            region(Gpa(0), MMIO32_START.0)?,
            region(MMIO32_END, high)?,
        ]),
        _ => Ok(vec![region(Gpa(0), size)?]),
    }
}

/// Places the device-memory window above `ram` and the 32-bit MMIO hole, and
/// below `max_gpa`.
pub fn device_window(ram: &[Range<Gpa>], max_gpa: Gpa) -> Result<Range<Gpa>, LayoutError> {
    let start = ram
        .iter()
        .try_fold(MMIO32_END, |floor, range| Some(floor.max(range.end()?)))
        .and_then(|floor| floor.align_up(DEVICE_MEMORY_ALIGN))
        .ok_or(LayoutError::Overflow)?;
    // `start` is at least 4 GiB, so `start - 1` cannot wrap.
    let available = max_gpa
        .0
        .checked_sub(start.0 - 1)
        .ok_or(LayoutError::Overflow)?;
    Range::new(start, available.min(DEVICE_MEMORY_MAX_SIZE)).ok_or(LayoutError::Overflow)
}

#[cfg(test)]
mod tests {
    use std::ops;

    use super::*;
    use crate::resources::AddressPool;

    fn gpa(range: ops::Range<u64>) -> Range<Gpa> {
        range.try_into().unwrap()
    }

    #[test]
    fn ram_stays_contiguous_below_the_mmio_hole() {
        assert_eq!(ram_regions(128 << 20).unwrap(), [gpa(0..128 << 20)]);
        assert_eq!(ram_regions(3 << 30).unwrap(), [gpa(0..3 << 30)]);
    }

    #[test]
    fn ram_continues_above_four_gib() {
        assert_eq!(
            ram_regions(4 << 30).unwrap(),
            [gpa(0..3 << 30), gpa(4 << 30..5 << 30)]
        );
        assert_eq!(
            ram_regions((3 << 30) + 0x1000).unwrap(),
            [gpa(0..3 << 30), gpa(4 << 30..(4 << 30) + 0x1000)]
        );
    }

    #[test]
    fn invalid_sizes_are_rejected() {
        assert_eq!(ram_regions(0), Err(LayoutError::Empty));
        assert_eq!(ram_regions(u64::MAX), Err(LayoutError::Unaligned(u64::MAX)));
        assert_eq!(ram_regions(0x1800), Err(LayoutError::Unaligned(0x1800)));
        assert_eq!(ram_regions(u64::MAX - 0xfff), Err(LayoutError::Overflow));
        // The largest layout ends exactly at the top of the address space.
        let largest = u64::MAX - (1 << 30) + 1;
        assert_eq!(ram_regions(largest).unwrap()[1].last(), Some(Gpa(u64::MAX)));
        assert_eq!(ram_regions(largest + 0x1000), Err(LayoutError::Overflow));
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
            let window = device_window(&ram, Gpa((1 << 46) - 1)).unwrap();
            assert_eq!(window, gpa(start..start + (1 << 40)));
            assert!(ram.iter().all(|range| !range.overlaps(window)));
        }
    }

    #[test]
    fn device_memory_is_clipped_to_the_physical_address_limit() {
        assert_eq!(
            device_window(&[gpa(0..63 << 30)], Gpa((1 << 36) - 1)).unwrap(),
            gpa(63 << 30..64 << 30)
        );
        let empty = device_window(&[gpa(0..128 << 20)], Gpa((1 << 32) - 1)).unwrap();
        assert_eq!(empty, gpa(4 << 30..4 << 30));
        assert!(
            AddressPool::new("device memory", empty)
                .allocate(1 << 30, DEVICE_MEMORY_ALIGN)
                .is_err()
        );
        assert_eq!(
            device_window(&[gpa(0..65 << 30)], Gpa((1 << 36) - 1)),
            Err(LayoutError::Overflow)
        );
        assert_eq!(
            device_window(&[gpa(0..u64::MAX)], Gpa(u64::MAX)),
            Err(LayoutError::Overflow)
        );
        let to_the_top = Range::inclusive(Gpa(1 << 32), Gpa(u64::MAX)).unwrap();
        assert_eq!(
            device_window(&[to_the_top], Gpa(u64::MAX)),
            Err(LayoutError::Overflow)
        );
        // Without RAM or a known width, the window still sits above the hole.
        assert_eq!(
            device_window(&[], Gpa(u64::MAX)).unwrap(),
            gpa(4 << 30..(4 << 30) + (1 << 40))
        );
    }

    #[test]
    fn fixed_ranges_are_typed_and_disjoint() {
        assert_eq!(COM1_PORTS.to_string(), "0x3f8..=0x3ff");
        assert_eq!(I8042_PORTS.to_string(), "0x60..=0x64");
        assert_eq!(LEGACY_GSIS.to_string(), "5..=23");
        assert!(!LEGACY_GSIS.contains(COM1_GSI));
        assert_eq!(SYSTEM_MEM.to_string(), "0x9fc00..=0xdffff");
        assert!(MMIO32_START.is_aligned(DEVICE_MEMORY_ALIGN));
        assert!(VIRTIO_MMIO_WINDOW.is_aligned(Alignment::new(VIRTIO_MMIO_LEN).unwrap()));
    }
}
