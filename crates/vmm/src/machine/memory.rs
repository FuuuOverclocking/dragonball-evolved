//! Guest RAM: where it sits in the guest-physical address space, what backs it
//! on the host, and the host mappings themselves.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::Arc;

use anyhow::{Context, Result};
use api::machine::MemoryBacking;
use vm_memory::mmap::MmapRegionBuilder;
use vm_memory::{FileOffset, GuestAddress, GuestRegionMmap};

use crate::devices::virtio::{GuestMemoryMmap, SharedGuestMemory};
use crate::resources::GpaRange;

/// Where guest RAM sits and what backs it. The ranges are guest ABI: a snapshot
/// records them and a restore maps the same ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryPlan {
    pub backing: MemoryBacking,
    /// Sorted and disjoint.
    pub regions: Vec<GpaRange>,
}

/// A machine's guest RAM: the host mappings, and the plan they were mapped from.
///
/// It has a single owner, the only one that can bind it to a VM. Views of the
/// bytes are cheap to clone, but cannot be bound, and the RAM itself cannot be
/// cloned, so it never backs two VMs:
///
/// ```compile_fail,E0277
/// fn twice(ram: vmm::machine::memory::GuestRam) {
///     let _copy = vmm::machine::memory::GuestRam::clone(&ram);
/// }
/// ```
#[derive(Debug)]
pub struct GuestRam {
    plan: MemoryPlan,
    memory: SharedGuestMemory,
}

impl GuestRam {
    /// Maps the planned regions, zero-filled. A shared region is backed by a
    /// memfd of its own, reachable through the region's file offset.
    pub fn map(plan: &MemoryPlan) -> Result<Self> {
        let mut mappings = Vec::with_capacity(plan.regions.len());
        for &region in &plan.regions {
            let len = usize::try_from(region.len()).context("guest memory region is too large")?;
            let builder = MmapRegionBuilder::<()>::new(len)
                .with_mmap_prot(libc::PROT_READ | libc::PROT_WRITE);
            let builder = match plan.backing {
                MemoryBacking::Anonymous => builder
                    .with_mmap_flags(libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE),
                MemoryBacking::Shared => builder
                    .with_mmap_flags(libc::MAP_SHARED | libc::MAP_NORESERVE)
                    .with_file_offset(FileOffset::new(memfd(region)?, 0)),
            };
            let mmap = builder
                .build()
                .with_context(|| format!("map guest memory region {region}"))?;
            let mapping = GuestRegionMmap::new(mmap, GuestAddress(region.start()))
                .context("guest memory region address overflows")?;
            mappings.push(mapping);
        }
        let memory =
            GuestMemoryMmap::from_regions(mappings).context("assemble guest memory regions")?;
        Ok(Self {
            plan: plan.clone(),
            memory: Arc::new(memory),
        })
    }

    /// The plan this RAM was mapped from.
    pub fn plan(&self) -> &MemoryPlan {
        &self.plan
    }

    /// A view of the bytes.
    pub fn memory(&self) -> &SharedGuestMemory {
        &self.memory
    }
}

/// A memfd holding exactly one shared region, sealed at that size so a process
/// it is handed to never sees it shrink or grow.
fn memfd(region: GpaRange) -> Result<File> {
    let name = CString::new(format!("guest-ram-{:#x}", region.start()))?;
    let create = |flags| {
        // SAFETY: `name` is a NUL-terminated string that outlives the call.
        let fd = unsafe {
            libc::memfd_create(
                name.as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING | flags,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor owned by nothing else.
        Ok(unsafe { File::from_raw_fd(fd) })
    };
    // Kernels from 6.3 warn about, and under vm.memfd_noexec=2 refuse, a memfd
    // that does not say whether it is executable; older kernels reject the flag.
    let file = match create(libc::MFD_NOEXEC_SEAL) {
        Err(error) if error.raw_os_error() == Some(libc::EINVAL) => create(0),
        created => created,
    }
    .context("create guest RAM memfd")?;
    file.set_len(region.len()).context("size guest RAM memfd")?;
    let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
    // SAFETY: `file` owns a live descriptor for the duration of the call.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
        return Err(io::Error::last_os_error()).context("seal guest RAM memfd");
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::FileExt;

    use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryRegion};

    use super::*;

    fn split(backing: MemoryBacking) -> MemoryPlan {
        MemoryPlan {
            backing,
            regions: vec![
                GpaRange::new(0, 0x2000),
                GpaRange::new(0x1_0000_0000, 0x1_0000_1000),
            ],
        }
    }

    #[test]
    fn maps_split_guest_memory_with_read_write_access() {
        for backing in [MemoryBacking::Anonymous, MemoryBacking::Shared] {
            let plan = split(backing);
            let ram = GuestRam::map(&plan).unwrap();
            assert_eq!(ram.plan(), &plan);
            let memory = ram.memory();
            let mapped = memory
                .iter()
                .map(|region| {
                    let start = region.start_addr().raw_value();
                    let range = GpaRange::new(start, start + region.len());
                    (range, region.file_offset().is_some())
                })
                .collect::<Vec<_>>();
            let shared = backing == MemoryBacking::Shared;
            assert_eq!(
                mapped,
                plan.regions
                    .iter()
                    .map(|&range| (range, shared))
                    .collect::<Vec<_>>()
            );
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

    #[test]
    fn shared_regions_are_reachable_through_a_sealed_memfd() {
        let ram = GuestRam::map(&split(MemoryBacking::Shared)).unwrap();
        let memory = ram.memory();
        memory
            .write_slice(b"guest", GuestAddress(0x1_0000_0010))
            .unwrap();
        let high = memory.iter().nth(1).unwrap();
        let file = high.file_offset().unwrap().file();
        // Another process mapping the fd would see the guest's writes.
        let mut bytes = [0; 5];
        file.read_exact_at(&mut bytes, 0x10).unwrap();
        assert_eq!(&bytes, b"guest");
        assert_eq!(file.metadata().unwrap().len(), 0x1000);
        assert!(file.set_len(0x2000).is_err(), "the memfd can still grow");
        assert!(file.set_len(0).is_err(), "the memfd can still shrink");
    }
}
