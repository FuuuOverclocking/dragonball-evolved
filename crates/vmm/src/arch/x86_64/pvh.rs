use linux_loader::loader::elf::start_info::{
    XEN_HVM_MEMMAP_TYPE_RAM, XEN_HVM_MEMMAP_TYPE_RESERVED, XEN_HVM_START_MAGIC_VALUE,
    hvm_memmap_table_entry, hvm_modlist_entry, hvm_start_info,
};
use thiserror::Error;
use vm_memory::ByteValued;

use super::layout::{
    HIMEM_START, MEMMAP_START, MODLIST_START, PVH_INFO_START, SYSTEM_MEM_END, SYSTEM_MEM_START,
};
use crate::resources::GpaRange;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestBlob {
    pub address: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Initrd {
    pub address: u64,
    pub size: u64,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PvhError {
    #[error("PVH boot requires at least 1 MiB of low guest memory")]
    InsufficientLowMemory,
}

pub fn boot_data(
    ram: &[GpaRange],
    cmdline_address: u64,
    initrd: Option<Initrd>,
) -> Result<Vec<GuestBlob>, PvhError> {
    let memmap = memory_map(ram)?;
    let modules = initrd
        .map(|initrd| {
            vec![hvm_modlist_entry {
                paddr: initrd.address,
                size: initrd.size,
                ..Default::default()
            }]
        })
        .unwrap_or_default();
    let start_info = hvm_start_info {
        magic: XEN_HVM_START_MAGIC_VALUE,
        version: 1,
        nr_modules: modules.len() as u32,
        modlist_paddr: if modules.is_empty() { 0 } else { MODLIST_START },
        cmdline_paddr: cmdline_address,
        memmap_paddr: MEMMAP_START,
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

pub fn memory_map(ram: &[GpaRange]) -> Result<Vec<hvm_memmap_table_entry>, PvhError> {
    ram.first()
        .filter(|region| region.start() == 0 && region.len() >= HIMEM_START)
        .ok_or(PvhError::InsufficientLowMemory)?;

    let mut entries = vec![
        entry(0, SYSTEM_MEM_START, XEN_HVM_MEMMAP_TYPE_RAM),
        entry(
            SYSTEM_MEM_START,
            SYSTEM_MEM_END - SYSTEM_MEM_START,
            XEN_HVM_MEMMAP_TYPE_RESERVED,
        ),
    ];
    for region in ram {
        let start = region.start().max(HIMEM_START);
        if start < region.end() {
            entries.push(entry(start, region.end() - start, XEN_HVM_MEMMAP_TYPE_RAM));
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
                entry(0, SYSTEM_MEM_START, XEN_HVM_MEMMAP_TYPE_RAM),
                entry(
                    SYSTEM_MEM_START,
                    SYSTEM_MEM_END - SYSTEM_MEM_START,
                    XEN_HVM_MEMMAP_TYPE_RESERVED,
                ),
                entry(
                    HIMEM_START,
                    (128 << 20) - HIMEM_START,
                    XEN_HVM_MEMMAP_TYPE_RAM
                ),
            ]
        );
    }

    #[test]
    fn memmap_preserves_the_mmio_hole() {
        let map = memory_map(&ram_regions(4 << 30).unwrap()).unwrap();
        assert_eq!(map[2].addr + map[2].size, MMIO32_START);
        assert_eq!(map[3].addr, MMIO32_END);
        assert_eq!(map[3].size, 1 << 30);
    }

    #[test]
    fn boot_data_describes_cmdline_memmap_and_initrd() {
        let blobs = boot_data(
            &ram_regions(128 << 20).unwrap(),
            CMDLINE_START,
            Some(Initrd {
                address: 120 << 20,
                size: 8 << 20,
            }),
        )
        .unwrap();
        assert_eq!(blobs.len(), 3);
        assert_eq!(blobs[0].address, PVH_INFO_START);
        assert_eq!(blobs[0].bytes.len(), size_of::<hvm_start_info>());
        let info = hvm_start_info::from_slice(&blobs[0].bytes).unwrap();
        assert_eq!(info.magic, XEN_HVM_START_MAGIC_VALUE);
        assert_eq!(info.version, 1);
        assert_eq!(info.nr_modules, 1);
        assert_eq!(info.modlist_paddr, MODLIST_START);
        assert_eq!(info.cmdline_paddr, CMDLINE_START);
        assert_eq!(info.memmap_paddr, MEMMAP_START);
        assert_eq!(blobs[2].address, MODLIST_START);
    }
}
