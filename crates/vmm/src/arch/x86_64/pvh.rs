use linux_loader::loader::elf::start_info::{
    XEN_HVM_MEMMAP_TYPE_RAM, XEN_HVM_MEMMAP_TYPE_RESERVED, XEN_HVM_START_MAGIC_VALUE,
    hvm_memmap_table_entry, hvm_modlist_entry, hvm_start_info,
};
use vm_memory::ByteValued;

use super::layout::{HIMEM_START, MEMMAP_START, MODLIST_START, PVH_INFO_START, SYSTEM_MEM};
use crate::space::{Gpa, Range};

/// Bytes to write into guest memory at `address`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestBlob {
    pub address: Gpa,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum PvhError {
    /// PVH boot requires at least 1 MiB of low guest memory
    InsufficientLowMemory,
}

pub fn boot_data(
    ram: &[Range<Gpa>],
    cmdline: Gpa,
    initrd: Option<Range<Gpa>>,
) -> Result<Vec<GuestBlob>, PvhError> {
    let memmap = memory_map(ram)?;
    let modules: Vec<_> = initrd
        .map(|initrd| hvm_modlist_entry {
            paddr: initrd.start().0,
            size: initrd.len(),
            ..Default::default()
        })
        .into_iter()
        .collect();
    let start_info = hvm_start_info {
        magic: XEN_HVM_START_MAGIC_VALUE,
        version: 1,
        nr_modules: modules.len() as u32,
        modlist_paddr: if modules.is_empty() {
            0
        } else {
            MODLIST_START.0
        },
        cmdline_paddr: cmdline.0,
        memmap_paddr: MEMMAP_START.0,
        memmap_entries: memmap.len() as u32,
        ..Default::default()
    };

    let mut blobs = vec![
        GuestBlob {
            address: PVH_INFO_START,
            bytes: bytes_of(&start_info),
        },
        GuestBlob {
            address: MEMMAP_START,
            bytes: bytes_of_slice(&memmap),
        },
    ];
    if !modules.is_empty() {
        blobs.push(GuestBlob {
            address: MODLIST_START,
            bytes: bytes_of_slice(&modules),
        });
    }
    Ok(blobs)
}

pub fn memory_map(ram: &[Range<Gpa>]) -> Result<Vec<hvm_memmap_table_entry>, PvhError> {
    ram.first()
        .filter(|region| region.start() == Gpa(0) && region.len() >= HIMEM_START.0)
        .ok_or(PvhError::InsufficientLowMemory)?;

    let mut entries = vec![
        entry(0, SYSTEM_MEM.start().0, XEN_HVM_MEMMAP_TYPE_RAM),
        entry(
            SYSTEM_MEM.start().0,
            SYSTEM_MEM.len(),
            XEN_HVM_MEMMAP_TYPE_RESERVED,
        ),
    ];
    for region in ram {
        // The part below 1 MiB is described above.
        let high = region.split_at(region.start().max(HIMEM_START));
        if let Some((_, high)) = high.filter(|(_, high)| !high.is_empty()) {
            entries.push(entry(high.start().0, high.len(), XEN_HVM_MEMMAP_TYPE_RAM));
        }
    }
    Ok(entries)
}

fn entry(addr: u64, size: u64, type_: u32) -> hvm_memmap_table_entry {
    hvm_memmap_table_entry {
        addr,
        size,
        type_,
        ..Default::default()
    }
}

fn bytes_of<T: ByteValued>(value: &T) -> Vec<u8> {
    value.as_slice().to_vec()
}

fn bytes_of_slice<T: ByteValued>(values: &[T]) -> Vec<u8> {
    values
        .iter()
        .flat_map(ByteValued::as_slice)
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use super::*;
    use crate::arch::x86_64::layout::{CMDLINE_START, MMIO32_END, MMIO32_START, ram_regions};

    #[test]
    fn memmap_reserves_system_data_and_skips_the_legacy_hole() {
        let map = memory_map(&ram_regions(128 << 20).unwrap()).unwrap();
        assert_eq!(
            map,
            [
                entry(0, 0x9_fc00, XEN_HVM_MEMMAP_TYPE_RAM),
                entry(0x9_fc00, 0xe_0000 - 0x9_fc00, XEN_HVM_MEMMAP_TYPE_RESERVED),
                entry(
                    HIMEM_START.0,
                    (128 << 20) - HIMEM_START.0,
                    XEN_HVM_MEMMAP_TYPE_RAM
                ),
            ]
        );
    }

    #[test]
    fn memmap_preserves_the_mmio_hole() {
        let map = memory_map(&ram_regions(4 << 30).unwrap()).unwrap();
        assert_eq!(map[2].addr + map[2].size, MMIO32_START.0);
        assert_eq!(map[3].addr, MMIO32_END.0);
        assert_eq!(map[3].size, 1 << 30);
    }

    #[test]
    fn memmap_needs_low_memory_and_reaches_the_top() {
        let small = Range::new(Gpa(0), HIMEM_START.0 - 0x1000).unwrap();
        assert_eq!(memory_map(&[small]), Err(PvhError::InsufficientLowMemory));
        assert_eq!(memory_map(&[]), Err(PvhError::InsufficientLowMemory));
        // Exactly 1 MiB leaves no RAM above it to describe.
        let low = Range::new(Gpa(0), HIMEM_START.0).unwrap();
        assert_eq!(memory_map(&[low]).unwrap().len(), 2);
        let top = Range::inclusive(MMIO32_END, Gpa(u64::MAX)).unwrap();
        let map = memory_map(&[low, top]).unwrap();
        assert_eq!(
            map[2],
            entry(MMIO32_END.0, top.len(), XEN_HVM_MEMMAP_TYPE_RAM)
        );
    }

    #[test]
    fn boot_data_describes_cmdline_memmap_and_initrd() {
        let initrd = Range::new(Gpa(120 << 20), 8 << 20).unwrap();
        let blobs = boot_data(
            &ram_regions(128 << 20).unwrap(),
            CMDLINE_START,
            Some(initrd),
        )
        .unwrap();
        assert_eq!(blobs.len(), 3);
        assert_eq!(blobs[0].address, PVH_INFO_START);
        assert_eq!(blobs[0].bytes.len(), size_of::<hvm_start_info>());
        let info = hvm_start_info::from_slice(&blobs[0].bytes).unwrap();
        assert_eq!(info.magic, XEN_HVM_START_MAGIC_VALUE);
        assert_eq!(info.version, 1);
        assert_eq!(info.nr_modules, 1);
        assert_eq!(info.modlist_paddr, MODLIST_START.0);
        assert_eq!(info.cmdline_paddr, CMDLINE_START.0);
        assert_eq!(info.memmap_paddr, MEMMAP_START.0);
        assert_eq!(blobs[2].address, MODLIST_START);
        let module = hvm_modlist_entry::from_slice(&blobs[2].bytes).unwrap();
        assert_eq!((module.paddr, module.size), (120 << 20, 8 << 20));
    }

    #[test]
    fn boot_data_without_initrd_has_no_modules() {
        let blobs = boot_data(&ram_regions(128 << 20).unwrap(), CMDLINE_START, None).unwrap();
        assert_eq!(blobs.len(), 2);
        let info = hvm_start_info::from_slice(&blobs[0].bytes).unwrap();
        assert_eq!((info.nr_modules, info.modlist_paddr), (0, 0));
        assert_eq!(info.memmap_entries, 3);
    }
}
