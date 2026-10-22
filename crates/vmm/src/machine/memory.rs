use std::sync::Arc;

use anyhow::{Context, Result, bail};
use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::VmFd;
use vm_memory::mmap::MmapRegionBuilder;
use vm_memory::{
    Address, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion, GuestRegionMmap,
};

use crate::arch::x86_64::layout::MemoryRegion;

pub type SharedGuestMemory = Arc<GuestMemoryMmap<()>>;

pub fn create(regions: &[MemoryRegion]) -> Result<SharedGuestMemory> {
    let mut mappings = Vec::with_capacity(regions.len());
    for region in regions {
        let end = region
            .start
            .checked_add(region.len)
            .context("guest memory region address overflows")?;
        let len = usize::try_from(region.len).context("guest memory region is too large")?;
        let mmap = MmapRegionBuilder::<()>::new(len)
            .with_mmap_prot(libc::PROT_READ | libc::PROT_WRITE)
            .with_mmap_flags(libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE)
            .build()
            .with_context(|| format!("map guest memory region {:#x}..{:#x}", region.start, end))?;
        let mapping = GuestRegionMmap::new(mmap, GuestAddress(region.start))
            .context("guest memory region address overflows")?;
        mappings.push(mapping);
    }
    let memory =
        GuestMemoryMmap::from_regions(mappings).context("assemble guest memory regions")?;
    Ok(Arc::new(memory))
}

/// # Safety
/// Mappings must outlive the VM and its vCPUs; slots must not overlap existing registrations.
pub unsafe fn register(vm: &VmFd, memory: &GuestMemoryMmap<()>, max_memslots: usize) -> Result<()> {
    let regions: Vec<_> = memory.iter().collect();
    if regions.len() > max_memslots {
        bail!(
            "guest memory needs {} KVM slots, host supports {max_memslots}",
            regions.len()
        );
    }
    for (slot, region) in regions.into_iter().enumerate() {
        let kvm_region = kvm_userspace_memory_region {
            slot: slot as u32,
            guest_phys_addr: region.start_addr().raw_value(),
            memory_size: region.len(),
            userspace_addr: region.as_ptr() as u64,
            flags: 0,
        };
        // SAFETY: `memory` owns this mapping until after `vm` is dropped, every
        // slot and GPA range is unique, and the region size is unchanged.
        unsafe { vm.set_user_memory_region(kvm_region) }
            .with_context(|| format!("register KVM memory slot {slot}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use vm_memory::{Bytes, GuestAddress};

    use super::*;

    #[test]
    fn creates_split_guest_memory_with_read_write_access() {
        let memory = create(&[
            MemoryRegion {
                start: 0,
                len: 0x2000,
            },
            MemoryRegion {
                start: 0x1_0000_0000,
                len: 0x1000,
            },
        ])
        .unwrap();
        memory.write_obj(0x1234_u32, GuestAddress(0x1000)).unwrap();
        memory
            .write_obj(0x5678_u32, GuestAddress(0x1_0000_0000))
            .unwrap();
        assert_eq!(
            memory.read_obj::<u32>(GuestAddress(0x1000)).unwrap(),
            0x1234
        );
        assert_eq!(
            memory.read_obj::<u32>(GuestAddress(0x1_0000_0000)).unwrap(),
            0x5678
        );
    }
}
