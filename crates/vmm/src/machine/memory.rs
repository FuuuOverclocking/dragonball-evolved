//! Guest RAM: where it sits in the guest-physical address space, and the host
//! mappings behind it.

use std::sync::Arc;

use anyhow::{Context, Result};
use vm_memory::mmap::MmapRegionBuilder;
use vm_memory::{
    Address, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion, GuestRegionMmap,
};

use crate::resources::GpaRange;

pub type SharedGuestMemory = Arc<GuestMemoryMmap<()>>;

/// Where guest RAM sits. Guest ABI: a snapshot records it and a restore maps the
/// same ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryPlan {
    /// Sorted and disjoint.
    pub regions: Vec<GpaRange>,
}

/// Maps the planned regions, zero-filled.
pub fn create(plan: &MemoryPlan) -> Result<SharedGuestMemory> {
    let mut mappings = Vec::with_capacity(plan.regions.len());
    for &region in &plan.regions {
        let len = usize::try_from(region.len()).context("guest memory region is too large")?;
        let mmap = MmapRegionBuilder::<()>::new(len)
            .with_mmap_prot(libc::PROT_READ | libc::PROT_WRITE)
            .with_mmap_flags(libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE)
            .build()
            .with_context(|| format!("map guest memory region {region}"))?;
        let mapping = GuestRegionMmap::new(mmap, GuestAddress(region.start()))
            .context("guest memory region address overflows")?;
        mappings.push(mapping);
    }
    let memory =
        GuestMemoryMmap::from_regions(mappings).context("assemble guest memory regions")?;
    Ok(Arc::new(memory))
}

/// The ranges `memory` maps, in address order.
pub fn regions(memory: &GuestMemoryMmap<()>) -> Vec<GpaRange> {
    memory
        .iter()
        .map(|region| {
            let start = region.start_addr().raw_value();
            GpaRange::new(start, start + region.len())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use vm_memory::{Bytes, GuestAddress};

    use super::*;

    #[test]
    fn creates_split_guest_memory_with_read_write_access() {
        let plan = MemoryPlan {
            regions: vec![
                GpaRange::new(0, 0x2000),
                GpaRange::new(0x1_0000_0000, 0x1_0000_1000),
            ],
        };
        let memory = create(&plan).unwrap();
        assert_eq!(regions(&memory), plan.regions);
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
