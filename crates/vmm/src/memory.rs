//! Guest memory, stage M1. Design notes: `ai-sessions/memory.md`.
//!
//! Three layers, from guest-visible to host-local:
//! - [`AddressMap`]: what each GPA range is for. Fixed when the VM is built,
//!   because E820 and ACPI publish it to the guest once.
//! - [`GpaLedger`] and [`MemoryLayout`]: allocations inside the map and the RAM
//!   regions that exist. Values; changed only by pure functions returning new
//!   values, and rebuilt on restore by replaying records, never by re-planning.
//! - [`GuestMemory`]: host mappings and KVM slots that realize a layout.
//!   Host-local, never serialized.
//!
//! Types and signatures only; every body is `todo!()`.
#![allow(dead_code, unused_variables)]

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::num::NonZeroU64;
use std::sync::Arc;

use kvm_ioctls::VmFd;
use vm_memory::{GuestAddress, GuestMemoryAtomic, GuestMemoryMmap, GuestRegionMmap};

// ---------------------------------------------------------------------------
// Addresses
// ---------------------------------------------------------------------------

/// Guest physical address. IOVA (behind virtio-iommu) and host virtual
/// addresses get their own types when they appear; none converts implicitly.
pub type Gpa = GuestAddress;

/// A non-empty GPA range `[start, start + len)` whose end does not overflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GpaRange {
    start: Gpa,
    len: NonZeroU64,
}

impl GpaRange {
    /// `None` if `len == 0` or `start + len` overflows.
    pub fn new(start: Gpa, len: u64) -> Option<Self> {
        todo!()
    }

    pub fn start(&self) -> Gpa {
        todo!()
    }

    pub fn len(&self) -> NonZeroU64 {
        todo!()
    }

    /// Exclusive.
    pub fn end(&self) -> Gpa {
        todo!()
    }

    pub fn contains(&self, other: &GpaRange) -> bool {
        todo!()
    }

    pub fn overlaps(&self, other: &GpaRange) -> bool {
        todo!()
    }
}

/// Guest RAM size requested by configuration: non-zero, a multiple of 1 MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RamSize(NonZeroU64);

impl RamSize {
    pub fn from_mib(mib: u32) -> Option<Self> {
        todo!()
    }

    pub fn bytes(&self) -> NonZeroU64 {
        todo!()
    }
}

/// Guest physical address width: `min(host MAXPHYADDR, configured cap)`, in `32..=52`.
/// Every window ends at or below `1 << bits`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysBits(u8);

impl PhysBits {
    pub fn new(bits: u8) -> Option<Self> {
        todo!()
    }
}

// ---------------------------------------------------------------------------
// Address map: fixed for the VM's lifetime
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    /// RAM present from boot. E820 RAM, minus the reserved grants inside it.
    BootRam,
    /// Reserved for RAM that appears at runtime (ACPI DIMM, virtio-mem). M2.
    HotplugRam,
    /// 32-bit PCI MMIO window, published in the host bridge `_CRS`.
    PciMmio32,
    /// PCIe ECAM, published in MCFG.
    PciEcam,
    /// 64-bit PCI MMIO window, published in the host bridge `_CRS`.
    PciMmio64,
    /// A fixed platform MMIO range.
    Platform(PlatformMmio),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformMmio {
    IoApic,
    LocalApic,
    /// Three pages for `KVM_SET_TSS_ADDR`; must not overlap anything the guest uses.
    KvmTss,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub kind: WindowKind,
    pub range: GpaRange,
}

/// The guest-visible GPA plan.
///
/// Invariants: windows are sorted and pairwise disjoint; all lie below
/// `1 << phys_bits`; `Platform` windows sit at their architectural addresses.
/// Gaps between windows are unassigned: accesses there are unhandled MMIO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressMap {
    phys_bits: PhysBits,
    windows: Vec<Window>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapRequest {
    pub boot_ram: RamSize,
    // M2: pub hotplug_ram: u64, sized at build because the window must be in ACPI from boot.
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// RAM plus the fixed windows does not fit under `1 << phys_bits`.
    AddressSpaceExhausted { needed: u64, available: u64 },
}

/// x86-64 fixed addresses. Close to Firecracker and Cloud Hypervisor; exact
/// values are not settled yet.
pub mod x86_64 {
    pub const MMIO32_START: u64 = 0xC000_0000;
    pub const ECAM_START: u64 = 0xE000_0000;
    pub const ECAM_SIZE: u64 = 256 << 20;
    pub const IOAPIC_START: u64 = 0xFEC0_0000;
    pub const LAPIC_START: u64 = 0xFEE0_0000;
    pub const KVM_TSS_START: u64 = 0xFFFB_D000;
    pub const RAM_64BIT_START: u64 = 1 << 32;
    /// `[LEGACY_HOLE_START, 1 MiB)`: EBDA, VGA and BIOS area, E820 reserved.
    pub const LEGACY_HOLE_START: u64 = 0x9_FC00;
    pub const HIMEM_START: u64 = 0x10_0000;
}

impl AddressMap {
    /// Pure and deterministic. Boot RAM fills `[0, MMIO32_START)` first and
    /// continues at 4 GiB; `PciMmio64` takes the rest of the address space
    /// after RAM, aligned to 1 GiB.
    pub fn plan(request: &MapRequest, phys_bits: PhysBits) -> Result<Self, PlanError> {
        todo!()
    }

    /// Re-admits a recorded map, checking only the invariants. A snapshot taken
    /// on a host with more physical address bits can fail here.
    pub fn replay(phys_bits: PhysBits, windows: Vec<Window>) -> Result<Self, PlanError> {
        todo!()
    }

    pub fn windows(&self) -> &[Window] {
        todo!()
    }

    pub fn window_of(&self, range: &GpaRange) -> Option<&Window> {
        todo!()
    }
}

// ---------------------------------------------------------------------------
// GPA ledger: allocations inside windows
// ---------------------------------------------------------------------------

/// Who a GPA grant belongs to. The owner decides which window kinds admit it
/// and how E820 reports it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Owner {
    /// `[LEGACY_HOLE_START, 1 MiB)`. E820 reserved.
    LegacyHole,
    /// PVH `hvm_start_info`, its module list and memory map.
    BootInfo,
    Cmdline,
    /// RSDP through DSDT. E820 ACPI.
    AcpiTables,
    /// Placed where the ELF program headers say.
    Kernel,
    Initrd,
    // Later: PCI BARs (in PciMmio32/64), memory devices (in HotplugRam), ...
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Fixed(Gpa),
    /// `align` is a power of two. `top_down` places at the highest fit (initrd, ACPI).
    Anywhere {
        align: u64,
        top_down: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRequest {
    pub owner: Owner,
    pub size: NonZeroU64,
    pub placement: Placement,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Grant {
    pub owner: Owner,
    pub range: GpaRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantError {
    NoSpace(GrantRequest),
    Overlaps {
        grant: Grant,
        existing: Grant,
    },
    /// The range is not inside a window that admits this owner.
    OutsideWindow(Grant),
}

/// Record of every GPA allocation.
///
/// Invariants: grants are pairwise disjoint; each lies inside one window whose
/// kind admits its owner.
/// Law: for any ledger `l` produced by `grant`, `replay(map, l.grants()) == Ok(l)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GpaLedger {
    grants: BTreeMap<Gpa, Grant>,
}

impl GpaLedger {
    /// Policy: chooses a range. Runs only when allocating for the first time.
    pub fn grant(
        &self,
        map: &AddressMap,
        request: &GrantRequest,
    ) -> Result<(Self, Grant), GrantError> {
        todo!()
    }

    /// Mechanism: re-admits recorded grants, checking invariants, running no policy.
    pub fn replay(
        map: &AddressMap,
        grants: impl IntoIterator<Item = Grant>,
    ) -> Result<Self, GrantError> {
        todo!()
    }

    pub fn grants(&self) -> impl Iterator<Item = &Grant> {
        self.grants.values()
    }
}

// ---------------------------------------------------------------------------
// Memory layout: the RAM regions that exist
// ---------------------------------------------------------------------------

/// Identity of a RAM region within one VM. Recorded in snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegionId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionRole {
    /// Boot RAM below the 32-bit MMIO hole, starting at GPA 0.
    BootLow,
    /// Boot RAM from 4 GiB.
    BootHigh,
    // M2: Dimm(slot), VirtioMem(device): admitted only by `HotplugRam`.
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RamRegion {
    pub id: RegionId,
    pub role: RegionRole,
    pub range: GpaRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutChange {
    Add(RamRegion),
    Remove(RegionId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    DuplicateId(RegionId),
    UnknownId(RegionId),
    Overlaps {
        region: RamRegion,
        existing: RegionId,
    },
    /// The range is not inside a window that admits this role.
    OutsideWindow(RamRegion),
}

/// Guest-visible RAM regions.
///
/// Invariants: regions are pairwise disjoint; each lies inside one window
/// whose kind admits its role. What is *populated* inside a region (virtio-mem
/// blocks, balloon pages) is device state, not layout.
///
/// Laws:
/// - `replay(map, cs) == cs.fold(empty, apply)`.
/// - `diff(a, b)` applied to `a` gives `b`; `diff(a, a)` is empty.
/// - building a VM is `replay(map, diff(empty, target))`; restore is the same
///   call with the recorded layout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryLayout {
    regions: BTreeMap<RegionId, RamRegion>,
}

impl MemoryLayout {
    /// Pure: the boot layout for `map`: one region per `BootRam` window.
    pub fn boot(map: &AddressMap) -> Self {
        todo!()
    }

    pub fn apply(&self, map: &AddressMap, change: &LayoutChange) -> Result<Self, LayoutError> {
        todo!()
    }

    pub fn replay(map: &AddressMap, changes: &[LayoutChange]) -> Result<Self, LayoutError> {
        todo!()
    }

    /// Removals first (so a range can be reused), then additions; each in id order.
    pub fn diff(&self, to: &Self) -> Vec<LayoutChange> {
        todo!()
    }
}

/// E820 entry type as seen by the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum E820Kind {
    Ram,
    Reserved,
    Acpi,
}

/// Pure: RAM regions minus grants whose owner the guest must not reuse
/// (legacy hole as reserved, ACPI tables as ACPI).
pub fn e820(layout: &MemoryLayout, ledger: &GpaLedger) -> Vec<(GpaRange, E820Kind)> {
    todo!()
}

// ---------------------------------------------------------------------------
// Host realization: host-local, never serialized
// ---------------------------------------------------------------------------

/// How regions are backed on the host. A host policy, not part of the record:
/// the restoring process may choose differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backing {
    AnonymousPrivate,
    // Later: memfd (needed for handover), hugepages, file (snapshot restore).
}

/// KVM memory slot number. Host-local: reassigned on restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SlotId(pub u32);

/// Free slot numbers below `KVM_CAP_NR_MEMSLOTS`. Pure data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotIds {
    free: BTreeSet<u32>,
}

impl SlotIds {
    pub fn new(limit: u32) -> Self {
        todo!()
    }

    pub fn take(&self) -> Option<(Self, SlotId)> {
        todo!()
    }

    pub fn give_back(&self, slot: SlotId) -> Self {
        todo!()
    }
}

/// The host mapping for one region. Shared with device views through the
/// `Arc`, so a stale view keeps it mapped rather than dangling.
pub struct Backed {
    region: RegionId,
    mapping: Arc<GuestRegionMmap>,
}

/// A region registered with KVM.
///
/// Owns the [`Backed`] its slot points to: the mapping cannot be released
/// while the slot exists. Dropping a `Registered` without `unregister` leaks
/// the mapping instead of unmapping it; a leak is safe, unmapping memory a live
/// slot points to is not.
pub struct Registered {
    backed: Backed,
    slot: SlotId,
}

/// A failed step hands back what it was given, unchanged.
pub struct Failed<T> {
    pub unchanged: T,
    pub error: io::Error,
}

/// Creates the host mapping for `region`.
pub fn back(region: &RamRegion, backing: &Backing) -> io::Result<Backed> {
    todo!()
}

impl Backed {
    /// `KVM_SET_USER_MEMORY_REGION`, `KVM_MR_CREATE`.
    pub fn register(self, vm: &VmFd, slot: SlotId) -> Result<Registered, Failed<Backed>> {
        todo!()
    }
}

impl Registered {
    /// `KVM_SET_USER_MEMORY_REGION` with size 0, `KVM_MR_DELETE`.
    pub fn unregister(self, vm: &VmFd) -> Result<(Backed, SlotId), Failed<Registered>> {
        todo!()
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        // Leak one reference to `mapping` unless `unregister` consumed us.
        todo!()
    }
}

/// A device's capability to access guest RAM. Holding one is the permission.
/// `memory()` on the inner value yields a consistent snapshot of the layout.
#[derive(Clone)]
pub struct MemoryView(GuestMemoryAtomic<GuestMemoryMmap>);

/// Guest memory of one VM: records plus their host realization.
///
/// Invariant: `realized` has exactly the ids of `layout`, and `view` maps
/// exactly the `realized` regions.
pub struct GuestMemory {
    map: AddressMap,
    ledger: GpaLedger,
    layout: MemoryLayout,
    realized: BTreeMap<RegionId, Registered>,
    slots: SlotIds,
    view: MemoryView,
}

impl GuestMemory {
    /// Realizes `layout` from nothing by committing `diff(empty, layout)`.
    /// Build and restore share this path; they differ only in where the
    /// records came from (planned vs. recorded).
    pub fn realize(
        vm: &VmFd,
        map: AddressMap,
        ledger: GpaLedger,
        layout: &MemoryLayout,
        backing: &Backing,
    ) -> anyhow::Result<Self> {
        todo!()
    }

    /// Applies one change atomically: on error, layout and realization are unchanged.
    /// - `Add`: `layout.apply` → `back` → `register` → publish the view.
    /// - `Remove`: `layout.apply` → publish the view without it → `unregister` → drop.
    pub fn commit(
        &mut self,
        vm: &VmFd,
        change: LayoutChange,
        backing: &Backing,
    ) -> anyhow::Result<()> {
        todo!()
    }

    pub fn view(&self) -> MemoryView {
        todo!()
    }

    /// The guest-visible part, for snapshots: map, ledger, layout.
    pub fn record(&self) -> (&AddressMap, &GpaLedger, &MemoryLayout) {
        todo!()
    }
}
