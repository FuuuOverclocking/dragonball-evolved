//! The KVM VM and the guest RAM bound to it.
//!
//! KVM keeps a VM alive while its fd or any of its vCPU fds is open, and the
//! VM's memslots point into the host mappings of guest RAM, so those mappings
//! must outlive every such fd. That is structural here: a [`KvmVm`] owns the
//! RAM it binds and gives it back only once nothing else holds the VM, and
//! every [`KvmVcpu`] holds its VM.

use std::ops::{Deref, DerefMut};
use std::os::fd::AsRawFd;
use std::range::RangeInclusive;
use std::sync::Arc;
use std::{io, mem};

use anyhow::{Context, Result, bail};
use kvm_bindings::{KVM_PIT_SPEAKER_DUMMY, kvm_pit_config, kvm_userspace_memory_region};
use kvm_ioctls::{VcpuFd, VmFd};
use logger::error_unlimited;
use vm_memory::{Address, GuestMemoryRegion};

use super::memory::{GuestRam, RamMapping};
use crate::KvmContext;
use crate::arch::x86_64::layout;
use crate::devices::virtio::SharedGuestMemory;
use crate::resources::{GpaRange, IdPool};

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
    /// Creates an unbound VM owning its RAM mappings.
    pub fn new(context: &KvmContext, ram: GuestRam) -> Result<(Arc<Self>, MemSlots)> {
        let pool = memslots(context.capabilities().max_memslots)?;
        let fd = context.kvm().create_vm().context("create KVM VM")?;
        fd.set_tss_address(layout::TSS_START as usize)
            .context("set TSS address")?;
        fd.create_irq_chip().context("create irqchip")?;
        fd.create_pit2(kvm_pit_config {
            flags: KVM_PIT_SPEAKER_DUMMY,
            ..Default::default()
        })
        .context("create PIT2")?;
        let vm = Arc::new(Self { fd, ram });
        let slots = MemSlots {
            vm: Arc::clone(&vm),
            pool,
        };
        Ok((vm, slots))
    }

    pub fn bind(
        self: &Arc<Self>,
        permit: SlotPermit,
        range: GpaRange,
    ) -> Result<Binding, (SlotPermit, anyhow::Error)> {
        let result = (|| {
            if !Arc::ptr_eq(self, &permit.vm) {
                bail!("memslot permit belongs to another VM");
            }
            if range.is_empty()
                || !range.start().is_multiple_of(layout::PAGE_SIZE)
                || !range.len().is_multiple_of(layout::PAGE_SIZE)
            {
                bail!("binding range must be non-empty and page-aligned");
            }
            let mapping = self.ram.mapping(range)?;
            let offset = usize::try_from(range.start() - mapping.start_addr().raw_value())?;
            let region = kvm_userspace_memory_region {
                slot: permit.slot.0,
                guest_phys_addr: range.start(),
                memory_size: range.len(),
                userspace_addr: mapping.as_ptr().wrapping_add(offset) as u64,
                flags: 0,
            };
            // SAFETY: the unique permit retains the VM and the binding retains its mapping until deletion succeeds.
            unsafe { self.fd.set_user_memory_region(region) }
                .with_context(|| format!("bind KVM memslot {} at {range}", permit.slot.0))?;
            Ok(mapping)
        })();
        match result {
            Ok(mapping) => Ok(Binding {
                permit: Some(permit),
                range,
                mapping,
            }),
            Err(error) => Err((permit, error)),
        }
    }

    pub fn fd(&self) -> &VmFd {
        &self.fd
    }

    /// A view of the guest RAM owned by this VM.
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

/// ```compile_fail,E0277
/// fn duplicate(slots: vmm::machine::vm::MemSlots) {
///     let _copy = vmm::machine::vm::MemSlots::clone(&slots);
/// }
/// ```
#[derive(Debug)]
pub struct MemSlots {
    vm: Arc<KvmVm>,
    pool: IdPool<MemSlot>,
}

impl MemSlots {
    pub fn allocate(&mut self, count: usize) -> Result<Vec<SlotPermit>> {
        let mut pool = self.pool.clone();
        let mut permits = Vec::new();
        for _ in 0..count {
            let (slot, next) = pool.allocate()?;
            pool = next;
            permits.push(SlotPermit {
                vm: Arc::clone(&self.vm),
                slot,
            });
        }
        self.pool = pool;
        Ok(permits)
    }
}

/// KVM accepts identical slot registrations, so binding needs a unique permit.
/// Dropping a permit burns its reservation until the VM is torn down.
///
/// ```compile_fail,E0277
/// fn duplicate(permit: vmm::machine::vm::SlotPermit) {
///     let _copy = vmm::machine::vm::SlotPermit::clone(&permit);
/// }
/// ```
#[derive(Debug)]
pub struct SlotPermit {
    vm: Arc<KvmVm>,
    slot: MemSlot,
}

impl SlotPermit {
    pub fn slot(&self) -> MemSlot {
        self.slot
    }

    /// ```compile_fail,E0382
    /// use vmm::machine::vm::SlotPermit;
    /// use vmm::resources::GpaRange;
    /// fn bind_twice(permit: SlotPermit, range: GpaRange) {
    ///     let _first = permit.bind(range);
    ///     let _second = permit.bind(range);
    /// }
    /// ```
    pub fn bind(self, range: GpaRange) -> Result<Binding, (Self, anyhow::Error)> {
        Arc::clone(&self.vm).bind(self, range)
    }
}

#[derive(Debug)]
pub struct Binding {
    permit: Option<SlotPermit>,
    range: GpaRange,
    mapping: RamMapping,
}

impl Binding {
    pub fn slot(&self) -> MemSlot {
        self.permit.as_ref().unwrap().slot
    }

    pub fn range(&self) -> GpaRange {
        self.range
    }

    pub fn unbind(mut self) -> Result<(SlotPermit, RamMapping), (Self, anyhow::Error)> {
        if let Err(error) = self.delete() {
            return Err((self, error));
        }
        Ok((self.permit.take().unwrap(), Arc::clone(&self.mapping)))
    }

    fn delete(&self) -> Result<()> {
        let permit = self.permit.as_ref().unwrap();
        let region = kvm_userspace_memory_region {
            slot: permit.slot.0,
            ..Default::default()
        };
        // SAFETY: zero size removes the slot; both the VM and mapping stay alive through the ioctl.
        unsafe { permit.vm.fd.set_user_memory_region(region) }
            .with_context(|| format!("unbind KVM memslot {}", permit.slot.0))
    }
}

impl Drop for Binding {
    fn drop(&mut self) {
        if self.permit.is_some()
            && let Err(error) = self.delete()
        {
            error_unlimited!("{error:#}; leaking mapping at {}", self.range);
            mem::forget(Arc::clone(&self.mapping));
        }
    }
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
    use crate::machine::build::create_vm;
    use crate::machine::memory::MemoryPlan;
    use crate::resources::ResourceError;

    fn plan(backing: MemoryBacking, context: &KvmContext) -> MemoryPlan {
        MemoryPlan::new(16 << 20, backing, context.capabilities()).unwrap()
    }

    const RUNTIME_RAM: GpaRange = GpaRange::new(0x1000_0000, 0x1000_2000);
    const CHUNK: GpaRange = GpaRange::new(0x1000_1000, 0x1000_2000);

    fn runtime_vm(
        context: &KvmContext,
        backing: MemoryBacking,
    ) -> (Arc<KvmVm>, Vec<Binding>, SlotPermit) {
        let ram = GuestRam::map(&plan(backing, context), &[RUNTIME_RAM]).unwrap();
        let (vm, bindings, mut pool) = create_vm(context, ram).unwrap();
        let permit = pool.allocate(1).unwrap().pop().unwrap();
        (vm, bindings, permit)
    }

    #[test]
    fn memslot_reservations_commit_only_when_all_slots_are_available() {
        if !Path::new("/dev/kvm").exists() {
            return;
        }
        let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
        let (vm, mut slots) = KvmVm::new(
            &context,
            GuestRam::map(&plan(MemoryBacking::Anonymous, &context), &[]).unwrap(),
        )
        .unwrap();
        slots.pool = memslots(2).unwrap().reserve(MemSlot(0)).unwrap();
        let original = slots.pool.clone();
        let error = slots.allocate(2).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ResourceError>(),
            Some(&ResourceError::Exhausted {
                pool: "KVM memslot"
            })
        );
        assert_eq!(slots.pool, original);
        assert!(slots.allocate(0).unwrap().is_empty());
        assert_eq!(slots.pool, original);
        let permits = slots.allocate(1).unwrap();
        assert_eq!(permits[0].slot(), MemSlot(1));
        drop(permits);
        assert!(slots.allocate(1).is_err());
        drop(slots);
        assert!(KvmVm::into_ram(vm).is_ok());
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
            let ram = GuestRam::map(&plan(backing, &context), &[]).unwrap();
            let (vm, bindings, _) = create_vm(&context, ram).unwrap();
            let memory = Arc::downgrade(vm.memory());
            let vcpu = vm.create_vcpu(0).unwrap();
            let kick = vcpu.try_clone().unwrap();
            vm.memory().write_obj(0x5au8, GuestAddress(0x1000)).unwrap();

            // Each vCPU fd keeps the VM, and so the memory, alive.
            drop(bindings);
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
            let (vm, bindings, _) =
                create_vm(&context, GuestRam::map(&plan, &[]).unwrap()).unwrap();
            vm.memory().write_obj(0x5au8, GuestAddress(0x1000)).unwrap();
            let vcpu = vm.create_vcpu(0).unwrap();
            let kick = vcpu.try_clone().unwrap();

            drop(bindings);
            let vm = KvmVm::into_ram(vm).expect_err("a vCPU still holds the VM");
            drop(vcpu);
            let vm = KvmVm::into_ram(vm).expect_err("a kick fd still holds the VM");
            drop(kick);
            let ram = KvmVm::into_ram(vm).unwrap();
            assert_eq!(ram.plan(), &plan);

            // The contents come along, and the RAM can back the next VM.
            let (vm, _bindings, _) = create_vm(&context, ram).unwrap();
            assert_eq!(
                vm.memory().read_obj::<u8>(GuestAddress(0x1000)).unwrap(),
                0x5a
            );
        }
    }

    #[test]
    fn failed_bind_preserves_the_permit_on_kvm() {
        if !Path::new("/dev/kvm").exists() {
            return;
        }
        let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
        let (vm, _boot, mut permit) = runtime_vm(&context, MemoryBacking::Anonymous);
        let slot = permit.slot();
        for range in [
            GpaRange::new(CHUNK.start(), CHUNK.start()),
            GpaRange::new(CHUNK.start() + 1, CHUNK.end()),
            GpaRange::new(CHUNK.start(), CHUNK.end() - 1),
            GpaRange::new(0x2000_0000, 0x2000_1000),
            GpaRange::new(RUNTIME_RAM.start() - 4096, RUNTIME_RAM.end()),
            GpaRange::new(0, 4096),
        ] {
            let (returned, _) = permit.bind(range).unwrap_err();
            assert_eq!(returned.slot(), slot);
            permit = returned;
        }
        let (other, _) = KvmVm::new(
            &context,
            GuestRam::map(&plan(MemoryBacking::Anonymous, &context), &[]).unwrap(),
        )
        .unwrap();
        let (returned, error) = other.bind(permit, CHUNK).unwrap_err();
        assert!(error.to_string().contains("another VM"));
        let binding = vm.bind(returned, CHUNK).unwrap();
        assert_eq!(binding.slot(), slot);
        assert_eq!(binding.range(), CHUNK);
        binding.unbind().unwrap();
    }

    #[test]
    fn binding_retains_vm_and_mapping_until_unbind_on_kvm() {
        if !Path::new("/dev/kvm").exists() {
            return;
        }
        let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
        for backing in [MemoryBacking::Anonymous, MemoryBacking::Shared] {
            let (vm, boot, permit) = runtime_vm(&context, backing);
            let binding = permit.bind(CHUNK).unwrap();
            let mapping = Arc::downgrade(&binding.mapping);
            let live_vm = Arc::downgrade(&vm);
            drop(boot);
            let vm = KvmVm::into_ram(vm).expect_err("binding still holds the VM");
            drop(vm);
            assert!(live_vm.upgrade().is_some());
            assert!(mapping.upgrade().is_some());
            let (permit, returned) = binding.unbind().unwrap();
            drop(permit);
            assert!(live_vm.upgrade().is_none());
            assert!(mapping.upgrade().is_some());
            drop(returned);
            assert!(mapping.upgrade().is_none());
        }
    }

    #[test]
    fn failed_unbind_returns_the_live_binding_for_retry_on_kvm() {
        if !Path::new("/dev/kvm").exists() {
            return;
        }
        let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
        let (vm, boot, permit) = runtime_vm(&context, MemoryBacking::Shared);
        let mut binding = permit.bind(CHUNK).unwrap();
        let slot = binding.slot();
        let mapping = Arc::downgrade(&binding.mapping);
        binding.permit.as_mut().unwrap().slot = MemSlot(u32::MAX);
        let (mut binding, error) = binding.unbind().unwrap_err();
        assert_eq!(
            error.downcast_ref::<kvm_ioctls::Error>().unwrap().errno(),
            libc::EINVAL
        );
        assert_eq!(binding.range(), CHUNK);
        assert!(mapping.upgrade().is_some());
        binding.permit.as_mut().unwrap().slot = slot;
        let (permit, returned) = binding.unbind().unwrap();
        assert_eq!(permit.slot(), slot);
        assert!(Arc::ptr_eq(&returned, &mapping.upgrade().unwrap()));
        drop(permit);
        drop(boot);
        drop(vm);
        drop(returned);
        assert!(mapping.upgrade().is_none());
    }

    #[test]
    fn failed_drop_leaks_mapping_instead_of_unmapping_on_kvm() {
        if !Path::new("/dev/kvm").exists() {
            return;
        }
        let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
        for backing in [MemoryBacking::Anonymous, MemoryBacking::Shared] {
            let (vm, boot, permit) = runtime_vm(&context, backing);
            let live_vm = Arc::downgrade(&vm);
            let mut binding = permit.bind(CHUNK).unwrap();
            let mapping = Arc::downgrade(&binding.mapping);
            binding.permit.as_mut().unwrap().slot = MemSlot(u32::MAX);
            drop(binding);
            drop(boot);
            drop(vm);
            assert!(live_vm.upgrade().is_none());
            assert!(
                mapping.upgrade().is_some(),
                "mapping was released after failed deletion"
            );
        }
    }
}
