//! The KVM VM and the guest memory bound to it.
//!
//! KVM keeps a VM alive while its fd or any of its vCPU fds is open, and the
//! VM's memslots point into the host mappings of guest memory, so those
//! mappings must outlive every such fd. That is structural here: a [`KvmVm`]
//! owns the memory it binds and gives it back only once nothing else holds the
//! VM, and every [`KvmVcpu`] holds its VM.

use std::ops::{Deref, DerefMut};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::{io, mem};

use kvm_bindings::{KVM_PIT_SPEAKER_DUMMY, kvm_pit_config, kvm_userspace_memory_region};
use kvm_ioctls::{VcpuFd, VmFd};
use logger::error_unlimited;

use crate::align::HostPageSize;
use crate::arch::x86_64::layout::TSS_START;
use crate::error::render_chain;
use crate::kvm_context::KvmContext;
use crate::memory::layout::WindowId;
use crate::memory::policy::SliceError;
use crate::memory::store::{MemoryStore, WindowMapping};
use crate::resources::{IdPool, IdPoolError};
use crate::space::{Gpa, MemSlot, ObjectOffset, Range};

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum KvmVmError {
    /// KVM reports {0} memslots, more than slot ids can name
    Memslots(usize),
    /// issue KVM_CREATE_VM
    Create(#[source] kvm_ioctls::Error),
    /// set the TSS address
    TssAddress(#[source] kvm_ioctls::Error),
    /// create the in-kernel irqchip
    Irqchip(#[source] kvm_ioctls::Error),
    /// create the in-kernel PIT
    Pit(#[source] kvm_ioctls::Error),
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum KvmVcpuError {
    /// create vCPU {index}
    Create {
        index: u8,
        #[source]
        source: kvm_ioctls::Error,
    },
    /// duplicate the vCPU fd
    Duplicate(#[source] io::Error),
    /// map the run structure of the duplicated vCPU fd
    MapRun(#[source] kvm_ioctls::Error),
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum BindError {
    /// memslot permit belongs to another VM
    ForeignPermit,
    /// window {0} is not part of this VM's memory
    UnknownWindow(WindowId),
    /// validate the memslot's object slice
    Slice(#[source] SliceError),
    /// bind KVM memslot {slot} to window {window}
    Kvm {
        slot: MemSlot,
        window: WindowId,
        #[source]
        source: kvm_ioctls::Error,
    },
}

/// unbind KVM memslot {slot}
#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub struct UnbindError {
    pub slot: MemSlot,
    #[source]
    pub source: kvm_ioctls::Error,
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum CreateVmError {
    /// create the VM
    Vm(#[from] KvmVmError),
    /// reserve memslots for the boot windows
    Memslots(#[from] IdPoolError<MemSlot>),
    /// bind the boot windows
    Bind(#[from] BindError),
}

/// A KVM VM, and the guest memory it owns.
#[derive(Debug)]
pub struct KvmVm {
    // Declared first, so the VM is closed before the memory it maps is released.
    fd: VmFd,
    memory: MemoryStore,
    host_page: HostPageSize,
}

impl KvmVm {
    /// Creates a VM, with its irqchip and PIT, that owns `memory` but binds
    /// none of it. The memslots come out once, as the only right to bind.
    /// On failure, closes any temporary VM fd before returning `memory`.
    pub fn new(
        context: &KvmContext,
        memory: MemoryStore,
    ) -> Result<(Arc<Self>, MemSlots), (MemoryStore, KvmVmError)> {
        Self::new_with_setup(context, memory, setup_vm)
    }

    fn new_with_setup(
        context: &KvmContext,
        memory: MemoryStore,
        setup: impl FnOnce(&mut VmFd) -> Result<(), KvmVmError>,
    ) -> Result<(Arc<Self>, MemSlots), (MemoryStore, KvmVmError)> {
        let max_memslots = context.capabilities().max_memslots;
        let Some(pool) = memslot_pool(max_memslots) else {
            return Err((memory, KvmVmError::Memslots(max_memslots)));
        };
        let mut fd = match context.kvm().create_vm() {
            Ok(fd) => fd,
            Err(error) => return Err((memory, KvmVmError::Create(error))),
        };
        if let Err(error) = setup(&mut fd) {
            drop(fd);
            return Err((memory, error));
        }
        let vm = Arc::new(Self {
            fd,
            memory,
            host_page: context.capabilities().page_size,
        });
        let slots = MemSlots {
            vm: Arc::clone(&vm),
            pool,
        };
        Ok((vm, slots))
    }

    /// Binds `window` of this VM's memory to the permit's slot. On failure the
    /// permit comes back unused.
    pub fn bind(
        self: &Arc<Self>,
        permit: SlotPermit,
        window: WindowId,
    ) -> Result<Binding, (SlotPermit, BindError)> {
        if !Arc::ptr_eq(self, &permit.vm) {
            return Err((permit, BindError::ForeignPermit));
        }
        let Some(descriptor) = self.memory.layout().window(window) else {
            return Err((permit, BindError::UnknownWindow(window)));
        };
        self.bind_slice(permit, window, descriptor.slice())
    }

    pub fn bind_slice(
        self: &Arc<Self>,
        permit: SlotPermit,
        window: WindowId,
        slice: Range<ObjectOffset>,
    ) -> Result<Binding, (SlotPermit, BindError)> {
        if !Arc::ptr_eq(self, &permit.vm) {
            return Err((permit, BindError::ForeignPermit));
        }
        if self.memory.layout().window(window).is_none() {
            return Err((permit, BindError::UnknownWindow(window)));
        }
        let checked = match self.memory.checked_slice(window, slice, self.host_page) {
            Ok(checked) => checked,
            Err(error) => return Err((permit, BindError::Slice(error))),
        };
        let slot = permit.slot;
        let region = kvm_userspace_memory_region {
            slot: slot.0,
            guest_phys_addr: checked.placement.start().0,
            memory_size: checked.len as u64,
            userspace_addr: checked.address as u64,
            flags: 0,
        };
        // SAFETY: the binding retains this live mapping, and its permit the VM, until the slot is deleted.
        if let Err(source) = unsafe { self.fd.set_user_memory_region(region) } {
            return Err((
                permit,
                BindError::Kvm {
                    slot,
                    window,
                    source,
                },
            ));
        }
        Ok(Binding {
            permit: Some(permit),
            window,
            slice,
            placement: checked.placement,
            mapping: Arc::clone(checked.mapping),
        })
    }

    pub fn fd(&self) -> &VmFd {
        &self.fd
    }

    /// The memory this VM owns.
    pub fn memory(&self) -> &MemoryStore {
        &self.memory
    }

    /// Closes the VM and hands its memory back, or returns the VM if anything
    /// else, such as a vCPU, a permit or a binding, still holds it.
    pub fn into_memory(self: Arc<Self>) -> Result<MemoryStore, Arc<Self>> {
        let Self { fd, memory, .. } = Arc::try_unwrap(self)?;
        drop(fd);
        Ok(memory)
    }

    pub fn create_vcpu(self: &Arc<Self>, index: u8) -> Result<KvmVcpu, KvmVcpuError> {
        let fd = self
            .fd
            .create_vcpu(u64::from(index))
            .map_err(|source| KvmVcpuError::Create { index, source })?;
        Ok(KvmVcpu {
            fd,
            vm: Arc::clone(self),
        })
    }
}

fn setup_vm(fd: &mut VmFd) -> Result<(), KvmVmError> {
    fd.set_tss_address(TSS_START.0 as usize)
        .map_err(KvmVmError::TssAddress)?;
    fd.create_irq_chip().map_err(KvmVmError::Irqchip)?;
    fd.create_pit2(kvm_pit_config {
        flags: KVM_PIT_SPEAKER_DUMMY,
        ..Default::default()
    })
    .map_err(KvmVmError::Pit)
}

/// Slots `0..max_memslots`; `None` if slot ids cannot name them all.
fn memslot_pool(max_memslots: usize) -> Option<IdPool<MemSlot>> {
    let slots = Range::new(MemSlot(0), u32::try_from(max_memslots).ok()?)?;
    Some(IdPool::new("KVM memslot", slots))
}

/// Creates a VM owning `memory` and binds its boot windows; hands back the VM,
/// the bindings and the memslots still free. On failure, releases every binding
/// and permit and closes the VM before returning `memory`.
#[allow(clippy::type_complexity)] // Keep the owned success and failure resources explicit.
pub fn create_vm(
    context: &KvmContext,
    memory: MemoryStore,
) -> Result<(Arc<KvmVm>, Vec<Binding>, MemSlots), (MemoryStore, CreateVmError)> {
    let boot: Vec<_> = memory.layout().boot_windows().map(|(id, _)| id).collect();
    let (vm, slots) =
        KvmVm::new(context, memory).map_err(|(memory, error)| (memory, error.into()))?;
    bind_boot(vm, slots, boot)
}

#[allow(clippy::type_complexity)] // The same per-stage ownership contract as `create_vm`.
fn bind_boot(
    vm: Arc<KvmVm>,
    mut slots: MemSlots,
    boot: Vec<WindowId>,
) -> Result<(Arc<KvmVm>, Vec<Binding>, MemSlots), (MemoryStore, CreateVmError)> {
    // This private build stage exposes no VM Arc and creates no vCPU. Once its
    // bindings, permits and slots are dropped, only `vm` can own the VM, so the
    // uniqueness assertion cannot fail. An invariant violation must not silently
    // discard the RAM as an ordinary build error.
    let into_memory = |vm: Arc<KvmVm>| {
        vm.into_memory()
            .expect("boot binding cleanup leaves the VM uniquely owned")
    };
    let mut permits = match slots.allocate(boot.len()) {
        Ok(permits) => permits.into_iter(),
        Err(error) => {
            drop(slots);
            return Err((into_memory(vm), error.into()));
        }
    };
    let mut bindings = Vec::new();
    for window in boot {
        let permit = permits.next().expect("one permit reserved per boot window");
        match permit.bind(window) {
            Ok(binding) => bindings.push(binding),
            Err((permit, error)) => {
                drop(bindings);
                drop(permit);
                drop(permits);
                drop(slots);
                return Err((into_memory(vm), error.into()));
            }
        }
    }
    Ok((vm, bindings, slots))
}

/// The right to reserve one VM's memslots. A VM hands out exactly one:
///
/// ```compile_fail,E0277
/// fn clone<T: Clone>() {}
/// clone::<vmm::vm::MemSlots>();
/// ```
#[derive(Debug)]
pub struct MemSlots {
    vm: Arc<KvmVm>,
    pool: IdPool<MemSlot>,
}

impl MemSlots {
    /// Reserves `count` slots, or none unless all of them are free.
    pub fn allocate(&mut self, count: usize) -> Result<Vec<SlotPermit>, IdPoolError<MemSlot>> {
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

/// One reserved memslot of one VM. KVM accepts identical slot registrations,
/// so binding needs a unique permit:
///
/// ```compile_fail,E0277
/// fn clone<T: Clone>() {}
/// clone::<vmm::vm::SlotPermit>();
/// ```
///
/// Dropping a permit burns its reservation until the VM is torn down.
#[derive(Debug)]
pub struct SlotPermit {
    vm: Arc<KvmVm>,
    slot: MemSlot,
}

impl SlotPermit {
    pub fn slot(&self) -> MemSlot {
        self.slot
    }

    /// Binds `window` of the permit's VM; see [`KvmVm::bind`]. Binding uses up
    /// the permit:
    ///
    /// ```compile_fail,E0382
    /// use vmm::memory::layout::WindowId;
    /// use vmm::vm::SlotPermit;
    /// fn bind_twice(permit: SlotPermit, window: WindowId) {
    ///     let _first = permit.bind(window);
    ///     let _second = permit.bind(window);
    /// }
    /// ```
    pub fn bind(self, window: WindowId) -> Result<Binding, (Self, BindError)> {
        Arc::clone(&self.vm).bind(self, window)
    }

    pub fn bind_slice(
        self,
        window: WindowId,
        slice: Range<ObjectOffset>,
    ) -> Result<Binding, (Self, BindError)> {
        Arc::clone(&self.vm).bind_slice(self, window, slice)
    }

    fn delete(&self) -> Result<(), UnbindError> {
        let region = kvm_userspace_memory_region {
            slot: self.slot.0,
            ..Default::default()
        };
        // SAFETY: a zero size deletes the slot; KVM stops reaching its memory before the ioctl returns.
        unsafe { self.vm.fd.set_user_memory_region(region) }.map_err(|source| UnbindError {
            slot: self.slot,
            source,
        })
    }
}

/// A window bound to a memslot. It retains the VM, through its permit, and the
/// window's mapping until KVM has deleted the slot.
#[derive(Debug)]
pub struct Binding {
    // Taken only by `unbind`, which consumes the binding.
    permit: Option<SlotPermit>,
    window: WindowId,
    slice: Range<ObjectOffset>,
    placement: Range<Gpa>,
    mapping: WindowMapping,
}

const HELD: &str = "a live binding holds its permit";

impl Binding {
    pub fn slot(&self) -> MemSlot {
        self.permit().slot
    }

    pub fn window(&self) -> WindowId {
        self.window
    }

    pub fn slice(&self) -> Range<ObjectOffset> {
        self.slice
    }

    pub fn placement(&self) -> Range<Gpa> {
        self.placement
    }

    /// Deletes the slot, then hands back the permit, free to bind again, and
    /// the mapping. If KVM refuses, the binding comes back live for a retry.
    pub fn unbind(mut self) -> Result<(SlotPermit, WindowMapping), (Self, UnbindError)> {
        if let Err(error) = self.permit().delete() {
            return Err((self, error));
        }
        let permit = self.permit.take().expect(HELD);
        Ok((permit, Arc::clone(&self.mapping)))
    }

    fn permit(&self) -> &SlotPermit {
        self.permit.as_ref().expect(HELD)
    }
}

impl Drop for Binding {
    fn drop(&mut self) {
        if let Some(permit) = &self.permit
            && let Err(error) = permit.delete()
        {
            // KVM may still reach the mapping, so it must never be unmapped.
            error_unlimited!(
                "{}; leaking the mapping of window {}",
                render_chain(&error),
                self.window
            );
            mem::forget(Arc::clone(&self.mapping));
        }
    }
}

/// A vCPU fd that keeps its VM, and with it the guest memory, alive.
#[derive(Debug)]
pub struct KvmVcpu {
    // Declared first, so the vCPU fd is closed before the VM can be released.
    fd: VcpuFd,
    vm: Arc<KvmVm>,
}

impl KvmVcpu {
    /// Another fd for the same vCPU, to reach it while its own thread is in
    /// `KVM_RUN`.
    pub fn try_clone(&self) -> Result<Self, KvmVcpuError> {
        // SAFETY: `self.fd` is live for the call, and the VcpuFd below owns the duplicate.
        let raw_fd = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if raw_fd < 0 {
            return Err(KvmVcpuError::Duplicate(io::Error::last_os_error()));
        }
        // SAFETY: `raw_fd` is a fresh duplicate of a vCPU fd of this VM, owned by nothing else.
        let fd =
            unsafe { self.vm.fd.create_vcpu_from_rawfd(raw_fd) }.map_err(KvmVcpuError::MapRun)?;
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
    use std::os::fd::RawFd;
    use std::path::Path;
    use std::sync::atomic::{AtomicI32, Ordering};

    use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryRegion};

    use super::*;
    use crate::align::BackingPageSize;
    use crate::memory::layout::{MemoryLayout, Object, ObjectId, Window, WindowKind, WindowOwner};
    use crate::memory::policy::{BackingKind, HostPolicy};
    use crate::space::{Gpa, ObjectOffset};

    const KINDS: [BackingKind; 2] = [BackingKind::AnonymousPrivate, BackingKind::SharedMemfd];
    const BOOT: WindowId = WindowId(0);
    const EXTRA: WindowId = WindowId(1);
    const EXTRA_START: Gpa = Gpa(0x1000_0000);

    /// The host's KVM, unless the host has none.
    fn kvm() -> Option<KvmContext> {
        Path::new("/dev/kvm")
            .exists()
            .then(|| KvmContext::from_dev_path("/dev/kvm").unwrap())
    }

    fn store(context: &KvmContext, layout: MemoryLayout, kind: BackingKind) -> MemoryStore {
        let host = context.capabilities();
        let policy = HostPolicy {
            kind,
            page_size: BackingPageSize::base(host.page_size),
        };
        MemoryStore::materialize(layout, policy, host).unwrap()
    }

    fn boot(context: &KvmContext, kind: BackingKind) -> MemoryStore {
        store(context, MemoryLayout::boot(16 << 20).unwrap(), kind)
    }

    /// 16 MiB of boot RAM, and two pages of another object above it.
    fn runtime_layout() -> MemoryLayout {
        let window = |object, len, start| {
            Window::new(
                ObjectId(object),
                Range::new(ObjectOffset(0), len).unwrap(),
                start,
                WindowOwner::Boot,
                WindowKind::BootRam,
            )
            .unwrap()
        };
        MemoryLayout::new(
            [
                (ObjectId(0), Object { capacity: 16 << 20 }),
                (ObjectId(1), Object { capacity: 0x2000 }),
            ],
            [
                (BOOT, window(0, 16 << 20, Gpa(0))),
                (EXTRA, window(1, 0x2000, EXTRA_START)),
            ],
        )
        .unwrap()
    }

    /// A VM with its first window bound, and a permit for the extra one.
    fn runtime_vm(context: &KvmContext, kind: BackingKind) -> (Arc<KvmVm>, Binding, SlotPermit) {
        let (vm, mut slots) = KvmVm::new(context, store(context, runtime_layout(), kind)).unwrap();
        let mut permits = slots.allocate(2).unwrap();
        let extra = permits.pop().unwrap();
        let boot = permits.pop().unwrap().bind(BOOT).unwrap();
        (vm, boot, extra)
    }

    /// Move the sole VM fd above ordinary allocations, with a fresh number for
    /// each observation, so parallel tests cannot reuse it before the close check.
    fn observe_fd(context: &KvmContext, fd: &mut VmFd) -> RawFd {
        static NEXT: AtomicI32 = AtomicI32::new(0x1_0000);
        // SAFETY: `fd` is live; fcntl returns a new, independently owned fd.
        let raw = unsafe {
            libc::fcntl(
                fd.as_raw_fd(),
                libc::F_DUPFD_CLOEXEC,
                NEXT.fetch_add(1, Ordering::Relaxed),
            )
        };
        assert!(raw >= 0, "{}", io::Error::last_os_error());
        // SAFETY: `raw` is a fresh duplicate of this VM fd, owned by nothing else.
        let observed = unsafe { context.kvm().create_vmfd_from_rawfd(raw) }.unwrap();
        *fd = observed; // Close the original; do not keep the VM alive via a duplicate.
        raw
    }

    fn assert_fd_closed(fd: RawFd) {
        // SAFETY: F_GETFD only checks the descriptor, whether open or closed.
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
    }

    fn observed_vm(context: &KvmContext, memory: MemoryStore) -> (Arc<KvmVm>, MemSlots, RawFd) {
        let mut fd = -1;
        let (vm, slots) = KvmVm::new_with_setup(context, memory, |vm_fd| {
            fd = observe_fd(context, vm_fd);
            setup_vm(vm_fd)
        })
        .unwrap();
        (vm, slots, fd)
    }

    fn check_memory_return(
        context: &KvmContext,
        kind: BackingKind,
        fail: impl FnOnce(MemoryStore) -> MemoryStore,
    ) {
        let memory = store(context, runtime_layout(), kind);
        let layout = memory.layout().clone();
        let policy = *memory.policy();
        let view = Arc::downgrade(memory.view());
        let mappings: Vec<_> = layout
            .windows()
            .map(|(id, _)| (id, Arc::downgrade(memory.mapping(id).unwrap())))
            .collect();
        let markers = [
            (GuestAddress(0x1000), 0x5a_u8),
            (GuestAddress(EXTRA_START.0 + 0x1000), 0xa5),
        ];
        for (address, byte) in markers {
            memory.view().write_obj(byte, address).unwrap();
        }

        let memory = fail(memory);
        assert_eq!(memory.layout(), &layout);
        assert_eq!(memory.policy(), &policy);
        assert!(Arc::ptr_eq(memory.view(), &view.upgrade().unwrap()));
        for (id, mapping) in &mappings {
            assert!(Arc::ptr_eq(
                memory.mapping(*id).unwrap(),
                &mapping.upgrade().unwrap()
            ));
        }
        for (address, byte) in markers {
            assert_eq!(memory.view().read_obj::<u8>(address).unwrap(), byte);
        }

        let (vm, bindings, slots) = create_vm(context, memory).unwrap();
        assert_eq!(bindings.len(), layout.boot_windows().count());
        for (address, byte) in markers {
            assert_eq!(vm.memory().view().read_obj::<u8>(address).unwrap(), byte);
        }
        drop((bindings, slots));
        drop(vm.into_memory().unwrap());
        assert!(view.upgrade().is_none());
        assert!(
            mappings
                .iter()
                .all(|(_, mapping)| mapping.upgrade().is_none())
        );
    }

    #[test]
    fn failed_fd_setup_closes_vm_before_returning_memory_on_kvm() {
        let Some(context) = kvm() else { return };
        for kind in KINDS {
            for stage in 0..3 {
                check_memory_return(&context, kind, |memory| {
                    let mut fd = -1;
                    let (memory, error) = KvmVm::new_with_setup(&context, memory, |vm_fd| {
                        fd = observe_fd(&context, vm_fd);
                        match stage {
                            0 => vm_fd
                                .set_tss_address(usize::MAX)
                                .map_err(KvmVmError::TssAddress),
                            1 => {
                                vm_fd.create_irq_chip().unwrap();
                                setup_vm(vm_fd)
                            }
                            _ => {
                                setup_vm(vm_fd).unwrap();
                                vm_fd
                                    .create_pit2(kvm_pit_config::default())
                                    .map_err(KvmVmError::Pit)
                            }
                        }
                    })
                    .unwrap_err();
                    match (stage, error) {
                        (0, KvmVmError::TssAddress(source)) => {
                            assert_eq!(source.errno(), libc::EINVAL)
                        }
                        (1, KvmVmError::Irqchip(source)) | (2, KvmVmError::Pit(source)) => {
                            assert_eq!(source.errno(), libc::EEXIST);
                        }
                        (_, error) => panic!("unexpected setup failure: {error}"),
                    }
                    assert_fd_closed(fd);
                    memory
                });
            }
        }
    }

    #[test]
    fn partial_boot_binding_failure_returns_memory_on_kvm() {
        let Some(context) = kvm() else { return };
        for kind in KINDS {
            check_memory_return(&context, kind, |memory| {
                let (vm, slots, fd) = observed_vm(&context, memory);
                let live_vm = Arc::downgrade(&vm);
                // The first binding succeeds; the second collides with it, with
                // a third permit still unconsumed when cleanup starts.
                let (memory, error) = bind_boot(vm, slots, vec![BOOT, BOOT, EXTRA]).unwrap_err();
                assert!(matches!(
                    error,
                    CreateVmError::Bind(BindError::Kvm { slot: MemSlot(1), window: BOOT, source })
                        if source.errno() == libc::EEXIST
                ));
                assert!(live_vm.upgrade().is_none());
                assert_fd_closed(fd);
                memory
            });
        }
    }

    #[test]
    fn incomplete_boot_permit_reservation_returns_memory_on_kvm() {
        let Some(context) = kvm() else { return };
        for kind in KINDS {
            check_memory_return(&context, kind, |memory| {
                let (vm, mut slots, fd) = observed_vm(&context, memory);
                let live_vm = Arc::downgrade(&vm);
                // Allocate one permit, then exhaust before reserving the second.
                slots.pool = memslot_pool(1).unwrap();
                let (memory, error) = bind_boot(vm, slots, vec![BOOT, EXTRA]).unwrap_err();
                assert!(matches!(
                    error,
                    CreateVmError::Memslots(IdPoolError::Exhausted {
                        pool: "KVM memslot"
                    })
                ));
                assert!(live_vm.upgrade().is_none());
                assert_fd_closed(fd);
                memory
            });
        }
    }

    #[test]
    fn memslots_are_bounded_by_what_kvm_provides() {
        let slots = memslot_pool(2).unwrap();
        let (first, slots) = slots.allocate().unwrap();
        let (second, slots) = slots.allocate().unwrap();
        assert_eq!((first, second), (MemSlot(0), MemSlot(1)));
        let exhausted = IdPoolError::Exhausted {
            pool: "KVM memslot",
        };
        assert_eq!(slots.allocate().unwrap_err(), exhausted);
        assert_eq!(memslot_pool(0).unwrap().allocate().unwrap_err(), exhausted);
        let most = usize::try_from(u32::MAX).unwrap();
        assert!(memslot_pool(most).is_some());
        assert!(memslot_pool(most + 1).is_none());
    }

    #[test]
    fn memslot_reservations_commit_only_when_all_slots_are_available() {
        let Some(context) = kvm() else { return };
        let (vm, mut slots) =
            KvmVm::new(&context, boot(&context, BackingKind::AnonymousPrivate)).unwrap();
        slots.pool = memslot_pool(2).unwrap().reserve(MemSlot(0)).unwrap();
        let original = slots.pool.clone();
        assert_eq!(
            slots.allocate(2).unwrap_err(),
            IdPoolError::Exhausted {
                pool: "KVM memslot"
            }
        );
        assert_eq!(slots.pool, original);
        assert!(slots.allocate(0).unwrap().is_empty());
        assert_eq!(slots.pool, original);
        let permits = slots.allocate(1).unwrap();
        assert_eq!(permits[0].slot(), MemSlot(1));
        // A dropped permit's slot is not handed out again.
        drop(permits);
        assert!(slots.allocate(1).is_err());
        drop(slots);
        assert!(KvmVm::into_memory(vm).is_ok());
    }

    #[test]
    fn create_vm_binds_every_boot_window() {
        let Some(context) = kvm() else { return };
        for kind in KINDS {
            let memory = store(&context, MemoryLayout::boot(4 << 30).unwrap(), kind);
            let (vm, bindings, mut slots) = create_vm(&context, memory).unwrap();
            let bound: Vec<_> = bindings
                .iter()
                .map(|binding| (binding.window(), binding.slot()))
                .collect();
            assert_eq!(
                bound,
                [(WindowId(0), MemSlot(0)), (WindowId(1), MemSlot(1))]
            );
            assert_eq!(slots.allocate(1).unwrap()[0].slot(), MemSlot(2));
            // KVM refuses a second binding of a bound window.
            let (_, error) = slots
                .allocate(1)
                .unwrap()
                .pop()
                .unwrap()
                .bind(WindowId(1))
                .unwrap_err();
            assert!(
                matches!(error, BindError::Kvm { source, .. } if source.errno() == libc::EEXIST)
            );
            drop((bindings, slots));
            assert!(KvmVm::into_memory(vm).is_ok());
        }
    }

    #[test]
    fn guest_memory_outlives_the_vm_and_its_vcpus_on_kvm() {
        let Some(context) = kvm() else { return };
        for kind in KINDS {
            let (vm, bindings, _) = create_vm(&context, boot(&context, kind)).unwrap();
            let view = Arc::downgrade(vm.memory().view());
            let mapping = Arc::downgrade(vm.memory().mapping(BOOT).unwrap());
            let vcpu = vm.create_vcpu(0).unwrap();
            let kick = vcpu.try_clone().unwrap();
            vm.memory()
                .view()
                .write_obj(0x5a_u8, GuestAddress(0x1000))
                .unwrap();

            // Each vCPU fd keeps the VM, and so the memory, alive.
            drop(bindings);
            drop(vm);
            drop(vcpu);
            assert!(view.upgrade().is_some());
            assert!(mapping.upgrade().is_some());
            drop(kick);
            assert!(view.upgrade().is_none());
            assert!(mapping.upgrade().is_none());
        }
    }

    #[test]
    fn memory_comes_back_only_once_every_vcpu_fd_is_closed_on_kvm() {
        let Some(context) = kvm() else { return };
        for kind in KINDS {
            let memory = boot(&context, kind);
            let (layout, policy) = (memory.layout().clone(), *memory.policy());
            let (vm, bindings, _) = create_vm(&context, memory).unwrap();
            vm.memory()
                .view()
                .write_obj(0x5a_u8, GuestAddress(0x1000))
                .unwrap();
            let vcpu = vm.create_vcpu(0).unwrap();
            let kick = vcpu.try_clone().unwrap();

            drop(bindings);
            let vm = KvmVm::into_memory(vm).expect_err("a vCPU still holds the VM");
            drop(vcpu);
            let vm = KvmVm::into_memory(vm).expect_err("a kick fd still holds the VM");
            drop(kick);
            let memory = KvmVm::into_memory(vm).unwrap();
            assert_eq!(memory.layout(), &layout);
            assert_eq!(memory.policy(), &policy);

            // The contents come along, and the memory can back the next VM.
            let (vm, _bindings, _) = create_vm(&context, memory).unwrap();
            assert_eq!(
                vm.memory()
                    .view()
                    .read_obj::<u8>(GuestAddress(0x1000))
                    .unwrap(),
                0x5a
            );
        }
    }

    fn hot_vm(context: &KvmContext, kind: BackingKind) -> (Arc<KvmVm>, MemSlots, u64) {
        let page = context.capabilities().page_size.alignment().get();
        let boot = MemoryLayout::boot(16 << 20).unwrap();
        let hot = Window::new(
            ObjectId(1),
            Range::new(ObjectOffset(2 * page), 4 * page).unwrap(),
            Gpa(1 << 32),
            WindowOwner::VirtioMem,
            WindowKind::VirtioMemRam,
        )
        .unwrap();
        let layout = MemoryLayout::new(
            boot.objects()
                .map(|(id, object)| (id, *object))
                .chain([(ObjectId(1), Object { capacity: 6 * page })]),
            boot.windows()
                .map(|(id, window)| (id, *window))
                .chain([(EXTRA, hot)]),
        )
        .unwrap();
        let (vm, slots) = KvmVm::new(context, store(context, layout, kind)).unwrap();
        (vm, slots, page)
    }

    #[test]
    fn slice_bind_uses_object_offsets_and_preserves_failed_permits_on_kvm() {
        let Some(context) = kvm() else { return };
        for kind in KINDS {
            let (vm, mut slots, page) = hot_vm(&context, kind);
            let mut permit = slots.allocate(1).unwrap().pop().unwrap();
            let slot = permit.slot();
            let invalid = [
                Range::new(ObjectOffset(2 * page), 0).unwrap(),
                Range::new(ObjectOffset(2 * page + 1), page).unwrap(),
                Range::new(ObjectOffset(2 * page), page + 1).unwrap(),
                Range::new(ObjectOffset(page), page).unwrap(),
                Range::new(ObjectOffset(5 * page), 2 * page).unwrap(),
            ];
            for (index, slice) in invalid.into_iter().enumerate() {
                let (returned, error) = permit.bind_slice(EXTRA, slice).unwrap_err();
                assert_eq!(returned.slot(), slot);
                assert!(Arc::ptr_eq(&returned.vm, &vm));
                match (index, error) {
                    (0, BindError::Slice(SliceError::Empty(EXTRA))) => {}
                    (1 | 2, BindError::Slice(SliceError::HostUnaligned { .. })) => {}
                    (3 | 4, BindError::Slice(SliceError::OutsideWindow { .. })) => {}
                    (_, error) => panic!("unexpected validation failure: {error}"),
                }
                permit = returned;
            }
            let slice = Range::new(ObjectOffset(3 * page), page).unwrap();
            let (returned, error) = vm.bind_slice(permit, WindowId(99), slice).unwrap_err();
            assert!(matches!(error, BindError::UnknownWindow(WindowId(99))));
            permit = returned;
            let (other, other_slots, _) = hot_vm(&context, kind);
            let (returned, error) = other.bind_slice(permit, EXTRA, invalid[0]).unwrap_err();
            assert!(matches!(error, BindError::ForeignPermit));
            assert_eq!(returned.slot(), slot);
            assert!(Arc::ptr_eq(&returned.vm, &vm));
            let expected_mapping = Arc::clone(vm.memory().mapping(EXTRA).unwrap());
            let checked = vm
                .memory()
                .checked_slice(EXTRA, slice, vm.host_page)
                .unwrap();
            assert_eq!(
                checked.address,
                expected_mapping.as_ptr().wrapping_add(page as usize)
            );
            assert_eq!(
                checked.placement,
                Range::new(Gpa((1 << 32) + page), page).unwrap()
            );
            let binding = returned.bind_slice(EXTRA, slice).unwrap();
            assert_eq!(binding.slot(), slot);
            assert_eq!(binding.slice(), slice);
            assert_eq!(binding.placement(), checked.placement);
            assert!(Arc::ptr_eq(&binding.mapping, &expected_mapping));
            let (permit, mapping) = binding.unbind().unwrap();
            assert_eq!(permit.slot(), slot);
            assert!(Arc::ptr_eq(&mapping, &expected_mapping));
            let whole = permit.bind(EXTRA).unwrap();
            assert_eq!(
                whole.slice(),
                vm.memory().layout().window(EXTRA).unwrap().slice()
            );
            assert_eq!(
                whole.placement(),
                vm.memory().layout().window(EXTRA).unwrap().placement()
            );
            whole.unbind().unwrap();
            drop((slots, other_slots, other));
            assert!(vm.into_memory().is_ok());
        }
    }

    #[test]
    fn disjoint_chunks_of_one_window_reuse_their_permits_on_kvm() {
        let Some(context) = kvm() else { return };
        for kind in KINDS {
            let (vm, mut slots, page) = hot_vm(&context, kind);
            let mut permits = slots.allocate(2).unwrap();
            let first = Range::new(ObjectOffset(2 * page), 2 * page).unwrap();
            let second = Range::new(ObjectOffset(4 * page), 2 * page).unwrap();
            let mut left = permits.pop().unwrap().bind_slice(EXTRA, first).unwrap();
            let right = vm
                .bind_slice(permits.pop().unwrap(), EXTRA, second)
                .unwrap();
            assert_eq!(left.window(), right.window());
            assert!(!left.placement().overlaps(right.placement()));
            assert!(Arc::ptr_eq(&left.mapping, &right.mapping));
            assert_eq!(left.mapping.len(), 4 * page);
            let slot = left.slot();
            left.permit.as_mut().unwrap().slot = MemSlot(u32::MAX);
            let (mut left, error) = left.unbind().unwrap_err();
            assert_eq!(error.source.errno(), libc::EINVAL);
            assert_eq!(left.slice(), first);
            assert_eq!(
                left.placement(),
                Range::new(Gpa(1 << 32), 2 * page).unwrap()
            );
            assert!(Arc::ptr_eq(&left.mapping, &right.mapping));
            left.permit.as_mut().unwrap().slot = slot;
            let (permit, mapping) = left.unbind().unwrap();
            let rebound = permit.bind_slice(EXTRA, first).unwrap();
            assert_eq!(rebound.slot(), slot);
            assert!(Arc::ptr_eq(&mapping, &rebound.mapping));
            rebound.unbind().unwrap();
            right.unbind().unwrap();
            drop((mapping, slots));
            assert!(vm.into_memory().is_ok());
        }
    }

    #[test]
    fn failed_bind_preserves_the_permit_on_kvm() {
        let Some(context) = kvm() else { return };
        let (vm, _boot, permit) = runtime_vm(&context, BackingKind::AnonymousPrivate);
        let slot = permit.slot();

        let (permit, error) = permit.bind(BOOT).unwrap_err();
        assert_eq!(permit.slot(), slot);
        assert!(
            matches!(error, BindError::Kvm { window: BOOT, source, .. } if source.errno() == libc::EEXIST)
        );
        let (permit, error) = permit.bind(WindowId(7)).unwrap_err();
        assert_eq!(permit.slot(), slot);
        assert!(matches!(error, BindError::UnknownWindow(WindowId(7))));

        // A permit binds only memory of its own VM.
        let (other, mut other_slots) =
            KvmVm::new(&context, boot(&context, BackingKind::AnonymousPrivate)).unwrap();
        let (permit, error) = other.bind(permit, BOOT).unwrap_err();
        assert_eq!(error.to_string(), "memslot permit belongs to another VM");
        let foreign = other_slots.allocate(1).unwrap().pop().unwrap();
        let (_, error) = vm.bind(foreign, EXTRA).unwrap_err();
        assert!(matches!(error, BindError::ForeignPermit));

        let binding = vm.bind(permit, EXTRA).unwrap();
        assert_eq!(binding.slot(), slot);
        assert_eq!(binding.window(), EXTRA);
        binding.unbind().unwrap();
    }

    #[test]
    fn binding_retains_vm_and_mapping_until_unbind_on_kvm() {
        let Some(context) = kvm() else { return };
        for kind in KINDS {
            let (vm, boot, permit) = runtime_vm(&context, kind);
            let binding = permit.bind(EXTRA).unwrap();
            assert_eq!(binding.mapping.start_addr().raw_value(), EXTRA_START.0);
            let mapping = Arc::downgrade(&binding.mapping);
            let live_vm = Arc::downgrade(&vm);
            drop(boot);
            let vm = KvmVm::into_memory(vm).expect_err("the binding still holds the VM");
            drop(vm);
            assert!(live_vm.upgrade().is_some());
            assert!(mapping.upgrade().is_some());

            let (permit, returned) = binding.unbind().unwrap();
            assert!(live_vm.upgrade().is_some());
            drop(permit);
            assert!(live_vm.upgrade().is_none());
            assert!(mapping.upgrade().is_some());
            drop(returned);
            assert!(mapping.upgrade().is_none());
        }
    }

    #[test]
    fn unbound_permits_bind_again_on_kvm() {
        let Some(context) = kvm() else { return };
        let (vm, _boot, permit) = runtime_vm(&context, BackingKind::SharedMemfd);
        let slot = permit.slot();
        let (permit, _) = permit.bind(EXTRA).unwrap().unbind().unwrap();
        let binding = permit.bind(EXTRA).unwrap();
        assert_eq!(binding.slot(), slot);
        drop(binding);
        drop(vm);
    }

    #[test]
    fn failed_unbind_returns_the_live_binding_for_retry_on_kvm() {
        let Some(context) = kvm() else { return };
        let (vm, boot, permit) = runtime_vm(&context, BackingKind::SharedMemfd);
        let mut binding = permit.bind(EXTRA).unwrap();
        let slot = binding.slot();
        let mapping = Arc::downgrade(&binding.mapping);
        binding.permit.as_mut().unwrap().slot = MemSlot(u32::MAX);

        let (mut binding, error) = binding.unbind().unwrap_err();
        assert_eq!(error.slot, MemSlot(u32::MAX));
        assert_eq!(error.source.errno(), libc::EINVAL);
        assert_eq!(binding.window(), EXTRA);
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
    fn failed_drop_leaks_the_mapping_instead_of_unmapping_on_kvm() {
        let Some(context) = kvm() else { return };
        for kind in KINDS {
            let (vm, boot, permit) = runtime_vm(&context, kind);
            let live_vm = Arc::downgrade(&vm);
            let mut binding = permit.bind(EXTRA).unwrap();
            let mapping = Arc::downgrade(&binding.mapping);
            binding.permit.as_mut().unwrap().slot = MemSlot(u32::MAX);
            drop(binding);
            drop(boot);
            drop(vm);
            assert!(live_vm.upgrade().is_none());
            assert!(
                mapping.upgrade().is_some(),
                "the mapping was released after a failed deletion"
            );
        }
    }
}
