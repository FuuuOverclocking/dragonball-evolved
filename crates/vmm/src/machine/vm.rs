//! The KVM VM and the guest RAM bound to it.
//!
//! KVM keeps a VM alive while its fd or any of its vCPU fds is open, and the
//! VM's memslots point into the host mappings of guest RAM, so those mappings
//! must outlive every such fd. That is structural here: a [`KvmVm`] owns the
//! RAM it binds and gives it back only once nothing else holds the VM, and
//! every [`KvmVcpu`] holds its VM.

use std::io;
use std::ops::{Deref, DerefMut};
use std::os::fd::AsRawFd;
use std::range::RangeInclusive;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use kvm_bindings::{KVM_PIT_SPEAKER_DUMMY, kvm_pit_config, kvm_userspace_memory_region};
use kvm_ioctls::{VcpuFd, VmFd};
use vm_memory::{Address, GuestMemoryBackend, GuestMemoryRegion};

use super::memory::GuestRam;
use crate::KvmContext;
use crate::arch::x86_64::layout;
use crate::devices::virtio::SharedGuestMemory;
use crate::resources::IdPool;

/// A KVM memory slot. Runtime only: a restore binds guest RAM to whichever
/// slots the new VM hands out, so slots never appear in a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MemSlot(pub u32);

impl From<u32> for MemSlot {
    fn from(slot: u32) -> Self {
        Self(slot)
    }
}

impl From<MemSlot> for u32 {
    fn from(slot: MemSlot) -> Self {
        slot.0
    }
}

#[derive(Debug)]
pub struct KvmVm {
    // Declared first, so the VM is closed before the RAM it maps is released.
    fd: VmFd,
    ram: GuestRam,
}

impl KvmVm {
    /// Creates a VM with the in-kernel interrupt controllers and PIT, and binds
    /// each region of `ram` to a memslot of its own.
    pub fn new(context: &KvmContext, ram: GuestRam) -> Result<Arc<Self>> {
        let fd = context.kvm().create_vm().context("create KVM VM")?;
        fd.set_tss_address(layout::TSS_START as usize)
            .context("set TSS address")?;
        fd.create_irq_chip().context("create irqchip")?;
        fd.create_pit2(kvm_pit_config {
            flags: KVM_PIT_SPEAKER_DUMMY,
            ..Default::default()
        })
        .context("create PIT2")?;
        let mut slots = memslots(context.capabilities().max_memslots)?;
        for region in ram.memory().iter() {
            let (slot, next) = slots.allocate().context("bind guest RAM")?;
            slots = next;
            let binding = kvm_userspace_memory_region {
                slot: slot.0,
                guest_phys_addr: region.start_addr().raw_value(),
                memory_size: region.len(),
                userspace_addr: region.as_ptr() as u64,
                flags: 0,
            };
            // SAFETY: the returned VM owns `ram` and gives it up, by drop or by
            // `into_ram`, only after its own fd and, through KvmVcpu, every vCPU
            // fd is closed. On an early return no vCPU exists yet, and the local
            // `fd` is dropped before the `ram` parameter. Each region gets a
            // distinct slot, and the regions of `ram` do not overlap.
            unsafe { fd.set_user_memory_region(binding) }
                .with_context(|| format!("bind guest RAM to KVM memslot {}", slot.0))?;
        }
        Ok(Arc::new(Self { fd, ram }))
    }

    pub fn fd(&self) -> &VmFd {
        &self.fd
    }

    /// A view of the guest RAM bound to this VM.
    pub fn memory(&self) -> &SharedGuestMemory {
        self.ram.memory()
    }

    /// Closes the VM and hands its RAM back, or returns the VM if anything else,
    /// such as a vCPU, still holds it.
    pub fn into_ram(self: Arc<Self>) -> Result<GuestRam, Arc<Self>> {
        let Self { fd, ram } = Arc::try_unwrap(self)?;
        drop(fd);
        Ok(ram)
    }

    pub fn create_vcpu(self: &Arc<Self>, index: u8) -> Result<KvmVcpu> {
        let fd = self
            .fd
            .create_vcpu(u64::from(index))
            .with_context(|| format!("create vCPU {index}"))?;
        Ok(KvmVcpu {
            fd,
            vm: Arc::clone(self),
        })
    }
}

fn memslots(max_memslots: usize) -> Result<IdPool<MemSlot>> {
    let Some(last) = max_memslots.checked_sub(1) else {
        bail!("KVM provides no memslots");
    };
    Ok(IdPool::new(
        "KVM memslot",
        RangeInclusive {
            start: MemSlot(0),
            last: MemSlot(u32::try_from(last).unwrap_or(u32::MAX)),
        },
    ))
}

/// A vCPU fd that keeps its VM, and with it the guest RAM, alive.
pub struct KvmVcpu {
    // Declared first, so the vCPU fd is closed before the VM can be released.
    fd: VcpuFd,
    vm: Arc<KvmVm>,
}

impl KvmVcpu {
    /// Another fd for the same vCPU, to reach it while its own thread is in
    /// `KVM_RUN`.
    pub fn try_clone(&self) -> Result<Self> {
        // SAFETY: `self.fd` is live and the new descriptor is owned by the VcpuFd below.
        let raw_fd = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if raw_fd < 0 {
            return Err(io::Error::last_os_error()).context("duplicate vCPU fd");
        }
        // SAFETY: `raw_fd` is a fresh duplicate of a vCPU fd of this VM.
        let fd = unsafe { self.vm.fd.create_vcpu_from_rawfd(raw_fd) }
            .context("construct duplicate vCPU fd")?;
        Ok(Self {
            fd,
            vm: Arc::clone(&self.vm),
        })
    }
}

impl Deref for KvmVcpu {
    type Target = VcpuFd;

    fn deref(&self) -> &VcpuFd {
        &self.fd
    }
}

impl DerefMut for KvmVcpu {
    fn deref_mut(&mut self) -> &mut VcpuFd {
        &mut self.fd
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use api::machine::MemoryBacking;
    use vm_memory::{Bytes, GuestAddress};

    use super::*;
    use crate::machine::memory::MemoryPlan;
    use crate::resources::ResourceError;

    fn plan(backing: MemoryBacking, context: &KvmContext) -> MemoryPlan {
        MemoryPlan::new(16 << 20, backing, context.capabilities()).unwrap()
    }

    #[test]
    fn memslots_are_bounded_by_what_kvm_provides() {
        let slots = memslots(2).unwrap();
        let (first, slots) = slots.allocate().unwrap();
        let (second, slots) = slots.allocate().unwrap();
        assert_eq!((first, second), (MemSlot(0), MemSlot(1)));
        assert_eq!(
            slots.allocate().unwrap_err(),
            ResourceError::Exhausted {
                pool: "KVM memslot"
            }
        );
        assert!(memslots(0).is_err());
    }

    #[test]
    fn guest_memory_outlives_the_vm_and_its_vcpus_on_kvm() {
        if !Path::new("/dev/kvm").exists() {
            return;
        }
        let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
        for backing in [MemoryBacking::Anonymous, MemoryBacking::Shared] {
            let ram = GuestRam::map(&plan(backing, &context)).unwrap();
            let vm = KvmVm::new(&context, ram).unwrap();
            let memory = Arc::downgrade(vm.memory());
            let vcpu = vm.create_vcpu(0).unwrap();
            let kick = vcpu.try_clone().unwrap();
            vm.memory().write_obj(0x5au8, GuestAddress(0x1000)).unwrap();

            // Each vCPU fd keeps the VM, and so the memory, alive.
            drop(vm);
            drop(vcpu);
            assert!(memory.upgrade().is_some());
            drop(kick);
            assert!(memory.upgrade().is_none());
        }
    }

    #[test]
    fn ram_comes_back_only_once_every_vcpu_fd_is_closed_on_kvm() {
        if !Path::new("/dev/kvm").exists() {
            return;
        }
        let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
        for backing in [MemoryBacking::Anonymous, MemoryBacking::Shared] {
            let plan = plan(backing, &context);
            let vm = KvmVm::new(&context, GuestRam::map(&plan).unwrap()).unwrap();
            vm.memory().write_obj(0x5au8, GuestAddress(0x1000)).unwrap();
            let vcpu = vm.create_vcpu(0).unwrap();
            let kick = vcpu.try_clone().unwrap();

            drop(vcpu);
            let vm = KvmVm::into_ram(vm).expect_err("a kick fd still holds the VM");
            drop(kick);
            let ram = KvmVm::into_ram(vm).unwrap();
            assert_eq!(ram.plan(), &plan);

            // The contents come along, and the RAM can back the next VM.
            let vm = KvmVm::new(&context, ram).unwrap();
            assert_eq!(
                vm.memory().read_obj::<u8>(GuestAddress(0x1000)).unwrap(),
                0x5a
            );
        }
    }
}
