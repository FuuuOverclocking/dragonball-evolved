use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::VmFd;
use vm_memory::{GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

pub type Memory = GuestMemoryMmap<()>;
pub type SharedMemory = Arc<Memory>;

/// One contiguous RAM slot, enough for the POC guest and its virtqueue.
#[derive(Debug)]
pub struct GuestRam {
    memory: SharedMemory,
    size: u64,
}

impl GuestRam {
    pub fn new(size: u64) -> Result<Self> {
        ensure!(size > 0, "guest memory must not be empty");
        let size = usize::try_from(size).context("guest memory size does not fit usize")?;
        let memory = Memory::from_ranges(&[(GuestAddress(0), size)]).context("map guest memory")?;
        Ok(Self {
            memory: Arc::new(memory),
            size: size as u64,
        })
    }

    pub fn memory(&self) -> SharedMemory {
        Arc::clone(&self.memory)
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub(crate) fn register(&self, vm: &VmFd) -> Result<()> {
        let userspace_addr = self
            .memory
            .get_host_address(GuestAddress(0))
            .context("locate guest memory backing")? as u64;
        let region = kvm_userspace_memory_region {
            slot: 0,
            guest_phys_addr: 0,
            memory_size: self.size,
            userspace_addr,
            flags: 0,
        };

        // SAFETY: `self.memory` owns the mapping described by `region` and the
        // enclosing machine keeps it alive until after all vCPUs and the VM fd are
        // dropped. Slot 0 is registered only once.
        unsafe { vm.set_user_memory_region(region) }.context("register guest memory with KVM")
    }
}

#[cfg(test)]
mod tests {
    use vm_memory::{Bytes, GuestAddress};

    use super::*;

    #[test]
    fn memory_is_shared_by_clone() {
        let ram = GuestRam::new(0x20_000).unwrap();
        let writer = ram.memory();
        let reader = ram.memory();

        writer.write_obj(0xfeed_u16, GuestAddress(0x1234)).unwrap();
        assert_eq!(
            reader.read_obj::<u16>(GuestAddress(0x1234)).unwrap(),
            0xfeed
        );
        assert_eq!(ram.size(), 0x20_000);
    }
}
