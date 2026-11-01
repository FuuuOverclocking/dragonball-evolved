//! The decisions that make a machine what the guest sees, taken once from its
//! spec and kept as a value.
//!
//! A plan is guest ABI only: how the host holds guest memory is a separate
//! [`HostPolicy`], decided alongside it but never part of it.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use api::machine::ConsoleConfig;

use crate::align::{Alignment, BackingPageSize};
use crate::arch::x86_64::layout::{
    CMDLINE_MAX_SIZE, DEVICE_MEMORY_ALIGN, LEGACY_GSIS, LayoutError, VIRTIO_MMIO_LEN,
    VIRTIO_MMIO_WINDOW, device_window,
};
use crate::kvm_context::HostCapabilities;
use crate::memory::hotplug::{GeometryError, MemGeometry};
use crate::memory::layout::{
    MemoryLayout, MemoryLayoutError, Object, ObjectId, Window, WindowId, WindowKind, WindowOwner,
};
use crate::memory::policy::{BackingKind, HostPolicy, HostPolicyError};
use crate::resources::{AddressPool, AddressPoolError, IdPool, IdPoolError};
use crate::space::{Gpa, Gsi, ObjectOffset, Range, Space};
use crate::spec::{DeviceSpec, MachineSpec, MemSpec, VIRTIO_MEM_ID};

/// The command line a guest gets unless the user gives one.
pub const DEFAULT_CMDLINE: &str =
    "console=ttyS0 reboot=k panic=1 pci=off i8042.noaux i8042.nomux i8042.dumbkbd";

// xAPIC IDs are 8 bits wide and 0xff is the broadcast ID.
const MAX_VCPUS: u8 = 254;
// The PVH kernel is loaded at 16 MiB and needs room above it.
const MIN_MEMORY_MIB: u32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum CmdlineError {
    /// kernel command line has {len} bytes, more than the {max} that fit
    TooLong { len: usize, max: usize },
    /// kernel command line has a non-printable character at byte {0}
    NotPrintable(usize),
}

/// A kernel command line that fits its slot in guest memory, NUL included,
/// and holds only printable ASCII.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelCmdline(String);

impl KernelCmdline {
    pub fn new(text: &str) -> Result<Self, CmdlineError> {
        if let Some(position) = text.bytes().position(|byte| !(b' '..=b'~').contains(&byte)) {
            return Err(CmdlineError::NotPrintable(position));
        }
        let max = CMDLINE_MAX_SIZE - 1;
        if text.len() > max {
            return Err(CmdlineError::TooLong {
                len: text.len(),
                max,
            });
        }
        Ok(Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The bytes the guest reads: the text and its terminating NUL.
    pub fn to_bytes_with_nul(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.0.len() + 1);
        bytes.extend_from_slice(self.0.as_bytes());
        bytes.push(0);
        bytes
    }
}

impl fmt::Display for KernelCmdline {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum PlanError {
    /// vCPU count {count} is not between 1 and {max}
    VcpuCount { count: u8, max: usize },
    /// no kernel is configured
    NoKernel,
    /// PVH boot needs at least 16 MiB of memory, got {0} MiB
    MemoryTooSmall(u32),
    /// lay out guest memory
    Layout(#[source] MemoryLayoutError),
    /// guest memory does not fit the host
    Policy(#[source] HostPolicyError),
    /// build the kernel command line
    Cmdline(#[source] CmdlineError),
    /// device id {0} is used twice
    DuplicateDevice(String),
    /// only one root disk is allowed
    MultipleRoots,
    /// only one virtio-mem device is allowed
    MultipleMem,
    /// virtio-mem device must have the stable id virtio-mem0, got {0}
    MemId(String),
    /// lay out the host-bounded device-memory aperture
    DeviceMemoryAperture(#[source] LayoutError),
    /// allocate or replay the fixed virtio-mem region
    DeviceMemory(#[source] AddressPoolError<Gpa>),
    /// memory object ids are exhausted
    ObjectIdsExhausted,
    /// memory window ids are exhausted
    WindowIdsExhausted,
    /// virtio-mem region overflows its object or guest address space
    MemRegionOverflow,
    /// validate virtio-mem geometry
    Geometry(#[source] GeometryError),
    /// window {0} does not match the recorded whole-object virtio-mem geometry
    GeometryMismatch(WindowId),
    /// virtio-mem region {0} must start on a 1 GiB boundary
    MemRegionUnaligned(Range<Gpa>),
    /// memory window {0} is neither boot RAM nor the recorded virtio-mem window
    UnexpectedWindow(WindowId),
    /// memory object {0} belongs to neither boot RAM nor the recorded virtio-mem region
    UnexpectedObject(ObjectId),
    /// boot windows and virtio-mem chunks need {count} memslots, more than the {max} KVM provides
    Memslots { count: usize, max: usize },
    /// boot-window and virtio-mem chunk memslot count overflows
    MemslotCountOverflow,
    /// allocate or replay a virtio-mmio window
    Mmio(#[from] AddressPoolError<Gpa>),
    /// allocate or replay a legacy interrupt line
    Gsi(#[from] IdPoolError<Gsi>),
    /// virtio-mmio grant {0} must be one aligned 4 KiB window
    InvalidGrant(Range<Gpa>),
    /// device grant {range} exceeds the host physical address limit {max}
    BeyondMaxGpa { range: Range<Gpa>, max: Gpa },
    /// device grant {0} overlaps guest RAM
    OverlapsRam(Range<Gpa>),
    /// guest probe order must name every device exactly once, with the root first and virtio-mem last
    ProbeOrder,
}

/// Only the resource pools currently consumed by the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceBook {
    virtio_mmio: AddressPool<Gpa>,
    legacy_gsi: IdPool<Gsi>,
}

impl ResourceBook {
    fn fresh() -> Self {
        Self {
            virtio_mmio: AddressPool::new("virtio-mmio", VIRTIO_MMIO_WINDOW),
            legacy_gsi: IdPool::new("legacy GSI", LEGACY_GSIS),
        }
    }
}

/// The register window and interrupt line granted to a virtio-mmio device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtioMmioGrant {
    pub mmio: Range<Gpa>,
    pub gsi: Gsi,
}

impl VirtioMmioGrant {
    fn allocate(book: &ResourceBook) -> Result<(Self, ResourceBook), PlanError> {
        let (mmio, virtio_mmio) = book
            .virtio_mmio
            .allocate(VIRTIO_MMIO_LEN, Alignment::new(VIRTIO_MMIO_LEN).unwrap())?;
        let (gsi, legacy_gsi) = book.legacy_gsi.allocate()?;
        Ok((
            Self { mmio, gsi },
            ResourceBook {
                virtio_mmio,
                legacy_gsi,
            },
        ))
    }

    fn reserve(self, book: &ResourceBook) -> Result<ResourceBook, PlanError> {
        if self.mmio.len() != VIRTIO_MMIO_LEN
            || !self
                .mmio
                .is_aligned(Alignment::new(VIRTIO_MMIO_LEN).unwrap())
        {
            return Err(PlanError::InvalidGrant(self.mmio));
        }
        Ok(ResourceBook {
            virtio_mmio: book.virtio_mmio.reserve(self.mmio)?,
            legacy_gsi: book.legacy_gsi.reserve(self.gsi)?,
        })
    }
}

/// A disk's backing and guest-visible placement; its ID is the map key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockPlan {
    path: PathBuf,
    read_only: bool,
    root: bool,
    grant: VirtioMmioGrant,
}

impl BlockPlan {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn read_only(&self) -> bool {
        self.read_only
    }
    pub fn root(&self) -> bool {
        self.root
    }
    pub fn grant(&self) -> VirtioMmioGrant {
        self.grant
    }
}

/// One fixed, whole-object memory region and its device transport grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemPlan {
    geometry: MemGeometry,
    grant: VirtioMmioGrant,
}

impl MemPlan {
    pub fn geometry(&self) -> MemGeometry {
        self.geometry
    }

    pub fn grant(&self) -> VirtioMmioGrant {
        self.grant
    }

    pub fn object_id(&self) -> ObjectId {
        self.geometry.window().object()
    }

    pub fn window_id(&self) -> WindowId {
        self.geometry.window_id()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicePlan {
    Block(BlockPlan),
    Mem(MemPlan),
}

impl DevicePlan {
    pub fn block(&self) -> Option<&BlockPlan> {
        match self {
            Self::Block(block) => Some(block),
            Self::Mem(_) => None,
        }
    }

    pub fn mem(&self) -> Option<&MemPlan> {
        match self {
            Self::Block(_) => None,
            Self::Mem(mem) => Some(mem),
        }
    }

    pub fn grant(&self) -> VirtioMmioGrant {
        match self {
            Self::Block(block) => block.grant(),
            Self::Mem(mem) => mem.grant(),
        }
    }
}

/// Only boot windows set the aperture floor; including the fixed memory region
/// itself would move that floor on replay and lose the recorded placement.
fn device_memory_pool(
    memory: &MemoryLayout,
    host: &HostCapabilities,
) -> Result<AddressPool<Gpa>, PlanError> {
    let boot_ranges: Vec<_> = memory
        .boot_windows()
        .map(|(_, window)| window.placement())
        .collect();
    let aperture =
        device_window(&boot_ranges, host.max_gpa()).map_err(PlanError::DeviceMemoryAperture)?;
    Ok(AddressPool::new("device memory", aperture))
}

fn add_hotplug_memory(
    boot: &MemoryLayout,
    spec: &MemSpec,
    host: &HostCapabilities,
) -> Result<(MemoryLayout, MemGeometry), PlanError> {
    let block = Alignment::new(spec.block_size).ok_or(PlanError::Geometry(
        GeometryError::BlockSize(spec.block_size),
    ))?;
    let chunk = Alignment::new(spec.chunk_size).ok_or(PlanError::Geometry(
        GeometryError::ChunkSize(spec.chunk_size),
    ))?;
    let alignment = if block.get() >= chunk.get() && block.get() >= DEVICE_MEMORY_ALIGN.get() {
        block
    } else if chunk.get() >= DEVICE_MEMORY_ALIGN.get() {
        chunk
    } else {
        DEVICE_MEMORY_ALIGN
    };
    let (region, _) = device_memory_pool(boot, host)?
        .allocate(spec.region_size, alignment)
        .map_err(PlanError::DeviceMemory)?;
    let object_id = ObjectId(
        boot.objects()
            .map(|(id, _)| id.0)
            .max()
            .map_or(Some(0), |id| id.checked_add(1))
            .ok_or(PlanError::ObjectIdsExhausted)?,
    );
    let window_id = WindowId(
        boot.windows()
            .map(|(id, _)| id.0)
            .max()
            .map_or(Some(0), |id| id.checked_add(1))
            .ok_or(PlanError::WindowIdsExhausted)?,
    );
    let slice =
        Range::new(ObjectOffset(0), spec.region_size).ok_or(PlanError::MemRegionOverflow)?;
    let window = Window::new(
        object_id,
        slice,
        region.start(),
        WindowOwner::VirtioMem,
        WindowKind::VirtioMemRam,
    )
    .ok_or(PlanError::MemRegionOverflow)?;
    let memory = MemoryLayout::new(
        boot.objects().map(|(id, object)| (id, *object)).chain([(
            object_id,
            Object {
                capacity: spec.region_size,
            },
        )]),
        boot.windows()
            .map(|(id, window)| (id, *window))
            .chain([(window_id, window)]),
    )
    .map_err(PlanError::Layout)?;
    let geometry = MemGeometry::new(
        &memory,
        window_id,
        spec.block_size,
        spec.chunk_size,
        BackingPageSize::base(host.page_size),
    )
    .map_err(PlanError::Geometry)?;
    Ok((memory, geometry))
}

/// What the guest sees of a machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachinePlan {
    vcpus: u8,
    memory: MemoryLayout,
    base_cmdline: KernelCmdline,
    devices: BTreeMap<String, DevicePlan>,
    probe_order: Vec<String>,
    com1: bool,
    kernel: PathBuf,
    initrd: Option<PathBuf>,
}

impl MachinePlan {
    /// Validates `spec` against `host`, then decides the guest ABI and, apart
    /// from it, how the host holds guest memory.
    pub fn new(
        spec: &MachineSpec,
        host: &HostCapabilities,
    ) -> Result<(Self, HostPolicy), PlanError> {
        let max = host.max_vcpus.min(usize::from(MAX_VCPUS));
        let vcpus = spec.cpus();
        if vcpus == 0 || usize::from(vcpus) > max {
            return Err(PlanError::VcpuCount { count: vcpus, max });
        }
        let kernel = spec.kernel().ok_or(PlanError::NoKernel)?;
        if spec.memory_mib() < MIN_MEMORY_MIB {
            return Err(PlanError::MemoryTooSmall(spec.memory_mib()));
        }
        let boot =
            MemoryLayout::boot(u64::from(spec.memory_mib()) << 20).map_err(PlanError::Layout)?;
        let policy = HostPolicy {
            kind: BackingKind::from(spec.memory_backing()),
            page_size: BackingPageSize::base(host.page_size),
        };
        policy.check(&boot, host).map_err(PlanError::Policy)?;
        let base_cmdline = KernelCmdline::new(kernel.cmdline.as_deref().unwrap_or(DEFAULT_CMDLINE))
            .map_err(PlanError::Cmdline)?;
        if spec
            .devices()
            .iter()
            .filter(|device| matches!(device, DeviceSpec::Mem(_)))
            .count()
            > 1
        {
            return Err(PlanError::MultipleMem);
        }
        if spec.memory_hotplug().is_some() && spec.blocks().any(|block| block.id == VIRTIO_MEM_ID) {
            return Err(PlanError::DuplicateDevice(VIRTIO_MEM_ID.into()));
        }
        let mut ids = HashSet::new();
        if let Some(device) = spec
            .devices()
            .iter()
            .find(|device| !ids.insert(device.id()))
        {
            return Err(PlanError::DuplicateDevice(device.id().to_owned()));
        }
        let mut blocks: Vec<_> = spec.blocks().collect();
        if blocks.iter().filter(|block| block.root).count() > 1 {
            return Err(PlanError::MultipleRoots);
        }
        blocks.sort_by_key(|block| !block.root);
        let (memory, geometry) = if let Some(mem) = spec.memory_hotplug() {
            let (memory, geometry) = add_hotplug_memory(&boot, mem, host)?;
            (memory, Some(geometry))
        } else {
            (boot, None)
        };
        policy.check(&memory, host).map_err(PlanError::Policy)?;
        let mut book = ResourceBook::fresh();
        let mut devices = BTreeMap::new();
        let mut probe_order = Vec::with_capacity(spec.devices().len());
        for block in blocks {
            let (grant, next) = VirtioMmioGrant::allocate(&book)?;
            book = next;
            probe_order.push(block.id.clone());
            devices.insert(
                block.id.clone(),
                DevicePlan::Block(BlockPlan {
                    path: block.path.clone(),
                    read_only: block.read_only,
                    root: block.root,
                    grant,
                }),
            );
        }
        if let Some(geometry) = geometry {
            let (grant, _) = VirtioMmioGrant::allocate(&book)?;
            probe_order.push(VIRTIO_MEM_ID.into());
            devices.insert(
                VIRTIO_MEM_ID.into(),
                DevicePlan::Mem(MemPlan { geometry, grant }),
            );
        }
        let plan = Self {
            vcpus,
            memory,
            base_cmdline,
            devices,
            probe_order,
            com1: spec.console() == ConsoleConfig::Stdio,
            kernel: kernel.path.clone(),
            initrd: kernel.initrd.clone(),
        };
        plan.replay(host)?;
        Ok((plan, policy))
    }

    pub fn vcpus(&self) -> u8 {
        self.vcpus
    }

    pub fn memory(&self) -> &MemoryLayout {
        &self.memory
    }

    pub fn devices(&self) -> &BTreeMap<String, DevicePlan> {
        &self.devices
    }

    pub fn memory_hotplug(&self) -> Option<&MemPlan> {
        self.devices.get(VIRTIO_MEM_ID).and_then(DevicePlan::mem)
    }

    /// Root first, then other disks in configuration order, then virtio-mem.
    /// This order is independent of map order and when hotplug was configured.
    pub fn probe_order(&self) -> &[String] {
        &self.probe_order
    }

    pub fn base_cmdline(&self) -> &KernelCmdline {
        &self.base_cmdline
    }

    /// Derived from immutable decisions; its final size was validated at construction.
    pub fn cmdline(&self) -> KernelCmdline {
        KernelCmdline(self.derive_cmdline())
    }

    fn derive_cmdline(&self) -> String {
        let mut text = self.base_cmdline.as_str().to_owned();
        for id in &self.probe_order {
            let device = &self.devices[id];
            let grant = device.grant();
            if !text.is_empty() {
                text.push(' ');
            }
            write!(
                text,
                "virtio_mmio.device=4K@{:#x}:{}",
                grant.mmio.start().0,
                grant.gsi.0
            )
            .unwrap();
            if let Some(block) = device.block()
                && block.root
            {
                text.push_str(if block.read_only {
                    " root=/dev/vda ro"
                } else {
                    " root=/dev/vda rw"
                });
            }
        }
        text
    }

    /// Validates recorded grants on this host without reallocating or changing the plan.
    pub fn replay(&self, host: &HostCapabilities) -> Result<ResourceBook, PlanError> {
        let max = host.max_vcpus.min(usize::from(MAX_VCPUS));
        let vcpus = self.vcpus;
        if vcpus == 0 || usize::from(vcpus) > max {
            return Err(PlanError::VcpuCount { count: vcpus, max });
        }
        HostPolicy {
            kind: BackingKind::AnonymousPrivate,
            page_size: BackingPageSize::base(host.page_size),
        }
        .check(&self.memory, host)
        .map_err(PlanError::Policy)?;
        let mut seen = HashSet::new();
        if self.probe_order.len() != self.devices.len()
            || self
                .probe_order
                .iter()
                .any(|id| !self.devices.contains_key(id) || !seen.insert(id))
        {
            return Err(PlanError::ProbeOrder);
        }
        let mem_count = self.devices.values().filter_map(DevicePlan::mem).count();
        if mem_count > 1 {
            return Err(PlanError::MultipleMem);
        }
        // A pre-hotplug plan may already use this disk ID. It only collides
        // when a memory device also claims the stable identity.
        if mem_count != 0
            && self
                .devices
                .get(VIRTIO_MEM_ID)
                .is_some_and(|device| device.block().is_some())
        {
            return Err(PlanError::DuplicateDevice(VIRTIO_MEM_ID.into()));
        }
        for (id, device) in &self.devices {
            if device.mem().is_some() && id != VIRTIO_MEM_ID {
                return Err(PlanError::MemId(id.clone()));
            }
        }
        if self.memory_hotplug().is_some()
            && self.probe_order.last().map(String::as_str) != Some(VIRTIO_MEM_ID)
        {
            return Err(PlanError::ProbeOrder);
        }
        let roots: Vec<_> = self
            .devices
            .iter()
            .filter(|(_, device)| device.block().is_some_and(|block| block.root))
            .collect();
        if roots.len() > 1 {
            return Err(PlanError::MultipleRoots);
        }
        if let Some((id, _)) = roots.first()
            && self.probe_order.first() != Some(id)
        {
            return Err(PlanError::ProbeOrder);
        }
        self.check_memory(host)?;
        let mut book = ResourceBook::fresh();
        for device in self.devices.values() {
            let grant = device.grant();
            book = grant.reserve(&book)?;
            if grant.mmio.last().is_some_and(|last| last > host.max_gpa()) {
                return Err(PlanError::BeyondMaxGpa {
                    range: grant.mmio,
                    max: host.max_gpa(),
                });
            }
            if self
                .memory
                .windows()
                .any(|(_, window)| window.placement().overlaps(grant.mmio))
            {
                return Err(PlanError::OverlapsRam(grant.mmio));
            }
        }
        KernelCmdline::new(&self.derive_cmdline()).map_err(PlanError::Cmdline)?;
        Ok(book)
    }

    fn check_memory(&self, host: &HostCapabilities) -> Result<(), PlanError> {
        let mem = self.memory_hotplug();
        if let Some(mem) = mem {
            let geometry = MemGeometry::new(
                &self.memory,
                mem.window_id(),
                mem.geometry.block_size(),
                mem.geometry.chunk_size(),
                BackingPageSize::base(host.page_size),
            )
            .map_err(PlanError::Geometry)?;
            let capacity = self
                .memory
                .objects()
                .find(|(id, _)| *id == mem.object_id())
                .map(|(_, object)| object.capacity);
            if geometry != mem.geometry
                || geometry.slice().start() != ObjectOffset(0)
                || capacity != Some(geometry.region().len())
                || self
                    .memory
                    .boot_windows()
                    .any(|(_, window)| window.object() == mem.object_id())
            {
                return Err(PlanError::GeometryMismatch(mem.window_id()));
            }
        }
        for (id, window) in self.memory.windows() {
            match (window.owner(), window.kind()) {
                (WindowOwner::Boot, WindowKind::BootRam) => {}
                (WindowOwner::VirtioMem, WindowKind::VirtioMemRam)
                    if mem.is_some_and(|mem| mem.window_id() == id) => {}
                _ => return Err(PlanError::UnexpectedWindow(id)),
            }
        }
        for (id, _) in self.memory.objects() {
            if !mem.is_some_and(|mem| mem.object_id() == id)
                && !self
                    .memory
                    .boot_windows()
                    .any(|(_, window)| window.object() == id)
            {
                return Err(PlanError::UnexpectedObject(id));
            }
        }
        if let Some(mem) = mem {
            let region = mem.geometry.region();
            if !region.start().is_aligned(DEVICE_MEMORY_ALIGN) {
                return Err(PlanError::MemRegionUnaligned(region));
            }
            device_memory_pool(&self.memory, host)?
                .reserve(region)
                .map_err(PlanError::DeviceMemory)?;
        }
        let mut count = self.memory.boot_windows().count();
        for mem in self.devices.values().filter_map(DevicePlan::mem) {
            let chunks = usize::try_from(mem.geometry.chunk_count())
                .map_err(|_| PlanError::MemslotCountOverflow)?;
            count = count
                .checked_add(chunks)
                .ok_or(PlanError::MemslotCountOverflow)?;
        }
        if count > host.max_memslots {
            return Err(PlanError::Memslots {
                count,
                max: host.max_memslots,
            });
        }
        Ok(())
    }

    /// Whether the guest has a COM1, which carries the console.
    pub fn has_com1(&self) -> bool {
        self.com1
    }

    pub fn kernel(&self) -> &Path {
        &self.kernel
    }

    pub fn initrd(&self) -> Option<&Path> {
        self.initrd.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use api::RawVmmOp;
    use api::block::{AddDisk, BlockDevConfig};
    use api::boot::{KernelConfig, SetKernel};
    use api::machine::{
        MemoryBacking, MemoryHotplugConfig, MemorySizeMib, SetConsole, SetCpus, SetMemory,
        SetMemoryBacking, SetMemoryHotplug, VcpuCount,
    };

    use super::*;
    use crate::memory::hotplug::{DEFAULT_BLOCK_SIZE, DEFAULT_CHUNK_SIZE};
    use crate::spec::BlockSpec;

    fn host() -> HostCapabilities {
        let mut host = HostCapabilities::fake(46, 32764, 4096);
        host.max_vcpus = 288;
        host
    }

    fn kernel(cmdline: Option<&str>) -> KernelConfig {
        KernelConfig {
            path: "/test/vmlinux".into(),
            cmdline: cmdline.map(Into::into),
            initrd: Some("/test/initrd".into()),
        }
    }

    fn spec(commands: impl IntoIterator<Item = api::VmmCommand>) -> MachineSpec {
        commands
            .into_iter()
            .fold(
                MachineSpec::default().apply(SetKernel(kernel(None)).into_vmm_command()),
                |spec, command| spec.and_then(|spec| spec.apply(command)),
            )
            .unwrap()
    }

    fn plan(spec: &MachineSpec) -> Result<MachinePlan, PlanError> {
        MachinePlan::new(spec, &host()).map(|(plan, _)| plan)
    }

    fn memory_hotplug(size_mib: u32) -> api::VmmCommand {
        SetMemoryHotplug(MemoryHotplugConfig {
            size_mib,
            block_size_mib: None,
            chunk_size_mib: None,
        })
        .into_vmm_command()
    }

    fn replace_mem_window(
        plan: &mut MachinePlan,
        window: Window,
        capacity: u64,
        record_geometry: bool,
    ) {
        let mem = plan.memory_hotplug().unwrap().clone();
        plan.memory = MemoryLayout::new(
            plan.memory.objects().map(|(id, object)| {
                if id == mem.object_id() {
                    (window.object(), Object { capacity })
                } else {
                    (id, *object)
                }
            }),
            plan.memory
                .windows()
                .map(|(id, old)| (id, if id == mem.window_id() { window } else { *old })),
        )
        .unwrap();
        if record_geometry {
            let geometry = MemGeometry::new(
                &plan.memory,
                mem.window_id(),
                mem.geometry.block_size(),
                mem.geometry.chunk_size(),
                BackingPageSize::base(host().page_size),
            )
            .unwrap();
            let DevicePlan::Mem(mem) = plan.devices.get_mut(VIRTIO_MEM_ID).unwrap() else {
                panic!("expected memory plan");
            };
            mem.geometry = geometry;
        }
    }

    #[test]
    fn the_plan_records_the_guest_abi_of_the_spec() {
        let spec = spec([
            SetCpus(VcpuCount(4)).into_vmm_command(),
            SetMemory(MemorySizeMib(4096)).into_vmm_command(),
        ]);
        let (plan, policy) = MachinePlan::new(&spec, &host()).unwrap();
        assert_eq!(plan.vcpus(), 4);
        assert_eq!(plan.memory(), &MemoryLayout::boot(4 << 30).unwrap());
        assert_eq!(
            plan.memory()
                .window(WindowId(1))
                .unwrap()
                .placement()
                .start(),
            Gpa(4 << 30)
        );
        assert_eq!(plan.kernel(), Path::new("/test/vmlinux"));
        assert_eq!(plan.initrd(), Some(Path::new("/test/initrd")));
        assert!(plan.has_com1());
        assert_eq!(
            policy,
            HostPolicy {
                kind: BackingKind::AnonymousPrivate,
                page_size: BackingPageSize::base(host().page_size),
            }
        );
        // The same spec on the same host makes the same plan.
        assert_eq!(MachinePlan::new(&spec, &host()).unwrap(), (plan, policy));
    }

    #[test]
    fn the_host_policy_follows_the_backing_and_stays_out_of_the_plan() {
        let anonymous = spec([]);
        let shared = spec([SetMemoryBacking(MemoryBacking::Shared).into_vmm_command()]);
        let (anonymous_plan, anonymous_policy) = MachinePlan::new(&anonymous, &host()).unwrap();
        let (shared_plan, shared_policy) = MachinePlan::new(&shared, &host()).unwrap();
        assert_eq!(anonymous_policy.kind, BackingKind::AnonymousPrivate);
        assert_eq!(shared_policy.kind, BackingKind::SharedMemfd);
        assert_eq!(anonymous_plan, shared_plan);
    }

    #[test]
    fn the_console_decides_whether_com1_exists() {
        let none = spec([SetConsole(ConsoleConfig::None).into_vmm_command()]);
        assert!(!plan(&none).unwrap().has_com1());
        assert!(plan(&spec([])).unwrap().has_com1());
    }

    #[test]
    fn the_default_cmdline_applies_only_without_a_user_one() {
        assert_eq!(plan(&spec([])).unwrap().cmdline().as_str(), DEFAULT_CMDLINE);
        let user = spec([SetKernel(kernel(Some("console=hvc0 quiet"))).into_vmm_command()]);
        let cmdline = plan(&user).unwrap().cmdline().clone();
        assert_eq!(cmdline.as_str(), "console=hvc0 quiet");
        assert_eq!(cmdline.to_bytes_with_nul(), b"console=hvc0 quiet\0");
        let empty = spec([SetKernel(kernel(Some(""))).into_vmm_command()]);
        assert_eq!(plan(&empty).unwrap().cmdline().as_str(), "");
    }

    #[test]
    fn the_cmdline_must_fit_with_its_nul_and_be_printable() {
        let longest = "x".repeat(CMDLINE_MAX_SIZE - 1);
        let fits = spec([SetKernel(kernel(Some(&longest))).into_vmm_command()]);
        assert_eq!(
            plan(&fits).unwrap().cmdline().to_bytes_with_nul().len(),
            CMDLINE_MAX_SIZE
        );
        let long = "x".repeat(CMDLINE_MAX_SIZE);
        let too_long = spec([SetKernel(kernel(Some(&long))).into_vmm_command()]);
        assert_eq!(
            plan(&too_long),
            Err(PlanError::Cmdline(CmdlineError::TooLong {
                len: CMDLINE_MAX_SIZE,
                max: CMDLINE_MAX_SIZE - 1,
            }))
        );
        assert_eq!(
            KernelCmdline::new("quiet\ninit=/bin/sh"),
            Err(CmdlineError::NotPrintable(5))
        );
        assert_eq!(
            crate::error::render_chain(&plan(&too_long).unwrap_err()),
            "build the kernel command line: kernel command line has 2048 bytes, more than the \
             2047 that fit"
        );
    }

    #[test]
    fn vcpu_counts_are_bounded_by_the_host_and_the_xapic() {
        let spec = spec([SetCpus(VcpuCount(254)).into_vmm_command()]);
        assert_eq!(plan(&spec).unwrap().vcpus(), 254);
        let mut small = host();
        small.max_vcpus = 8;
        assert_eq!(
            MachinePlan::new(&spec, &small),
            Err(PlanError::VcpuCount { count: 254, max: 8 })
        );
        assert_eq!(
            MachinePlan::new(&spec, &small).unwrap_err().to_string(),
            "vCPU count 254 is not between 1 and 8"
        );
    }

    #[test]
    fn replay_rechecks_vcpu_counts_against_the_current_host() {
        let mut source = host();
        source.max_vcpus = 2;
        let spec = spec([SetCpus(VcpuCount(2)).into_vmm_command()]);
        let (plan, _) = MachinePlan::new(&spec, &source).unwrap();
        assert!(plan.replay(&source).is_ok());
        let mut target = host();
        target.max_vcpus = 1;
        assert_eq!(
            plan.replay(&target),
            Err(PlanError::VcpuCount { count: 2, max: 1 })
        );
    }

    #[test]
    fn replay_rejects_vcpu_counts_outside_the_xapic_range() {
        let original = plan(&spec([])).unwrap();
        for count in [0, 255] {
            let mut bad = original.clone();
            bad.vcpus = count;
            assert_eq!(
                bad.replay(&host()),
                Err(PlanError::VcpuCount { count, max: 254 })
            );
        }
        let mut largest = original;
        largest.vcpus = 254;
        assert!(largest.replay(&host()).is_ok());
    }

    #[test]
    fn a_kernel_and_16_mib_of_memory_are_required() {
        assert_eq!(plan(&MachineSpec::default()), Err(PlanError::NoKernel));
        let small = spec([SetMemory(MemorySizeMib(15)).into_vmm_command()]);
        assert_eq!(plan(&small), Err(PlanError::MemoryTooSmall(15)));
        let smallest = spec([SetMemory(MemorySizeMib(16)).into_vmm_command()]);
        assert_eq!(
            plan(&smallest).unwrap().memory(),
            &MemoryLayout::boot(16 << 20).unwrap()
        );
    }

    #[test]
    fn memory_must_fit_the_host() {
        let large = spec([SetMemory(MemorySizeMib(64 << 10)).into_vmm_command()]);
        let narrow = HostCapabilities::fake(36, 8, 4096);
        assert!(matches!(
            MachinePlan::new(&large, &narrow),
            Err(PlanError::Policy(HostPolicyError::BeyondMaxGpa { .. }))
        ));
        let split = spec([SetMemory(MemorySizeMib(4096)).into_vmm_command()]);
        assert_eq!(
            MachinePlan::new(&split, &HostCapabilities::fake(46, 1, 4096)),
            Err(PlanError::Policy(HostPolicyError::Memslots {
                count: 2,
                max: 1
            }))
        );
        let huge_pages = HostCapabilities::fake(46, 8, 1 << 21);
        let odd = spec([SetMemory(MemorySizeMib(17)).into_vmm_command()]);
        assert!(matches!(
            MachinePlan::new(&odd, &huge_pages),
            Err(PlanError::Policy(HostPolicyError::HostUnaligned { .. }))
        ));
    }

    fn disk(id: &str) -> api::VmmCommand {
        AddDisk(BlockDevConfig {
            id: Some(id.into()),
            path: "/test/disk".into(),
            read_only: false,
            root: false,
        })
        .into_vmm_command()
    }

    #[test]
    fn block_plans_are_keyed_by_id_with_root_first_probe_order() {
        let mut root = AddDisk(BlockDevConfig {
            id: Some("root".into()),
            path: "/test/root".into(),
            read_only: true,
            root: true,
        });
        let configured = spec([disk("z"), root.clone().into_vmm_command(), disk("a")]);
        let plan = plan(&configured).unwrap();
        assert_eq!(plan.probe_order(), ["root", "z", "a"]);
        assert_eq!(
            plan.devices()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["a", "root", "z"]
        );
        let mut grants = Vec::new();
        for (index, id) in plan.probe_order().iter().enumerate() {
            let block = plan.devices()[id].block().unwrap();
            let grant = block.grant();
            assert_eq!(grant.mmio.start(), Gpa(0xc000_0000 + index as u64 * 0x1000));
            assert_eq!(grant.mmio.len(), 0x1000);
            assert!(grant.mmio.is_aligned(Alignment::new(0x1000).unwrap()));
            assert!(VIRTIO_MMIO_WINDOW.contains_range(grant.mmio));
            assert_eq!(grant.gsi, Gsi(5 + index as u32));
            assert!(grants.iter().all(|previous: &VirtioMmioGrant| {
                !previous.mmio.overlaps(grant.mmio) && previous.gsi != grant.gsi
            }));
            assert!(
                plan.memory()
                    .windows()
                    .all(|(_, window)| !window.placement().overlaps(grant.mmio))
            );
            grants.push(grant);
        }
        assert_eq!(
            plan.cmdline().as_str(),
            format!(
                "{DEFAULT_CMDLINE} virtio_mmio.device=4K@0xc0000000:5 root=/dev/vda ro virtio_mmio.device=4K@0xc0001000:6 virtio_mmio.device=4K@0xc0002000:7"
            )
        );
        assert_eq!(plan.replay(&host()).unwrap(), {
            let mut book = ResourceBook::fresh();
            for grant in grants {
                book = grant.reserve(&book).unwrap();
            }
            book
        });
        root.0.read_only = false;
        assert!(
            self::plan(&spec([root.into_vmm_command()]))
                .unwrap()
                .cmdline()
                .as_str()
                .ends_with("root=/dev/vda rw")
        );
    }

    #[test]
    fn pools_and_the_host_bound_device_grants() {
        let many = |count| spec((0..count).map(|n| disk(&format!("disk{n}"))));
        assert_eq!(
            plan(&many(19)).unwrap().devices()["disk18"]
                .block()
                .unwrap()
                .grant()
                .gsi,
            Gsi(23)
        );
        assert_eq!(
            plan(&many(20)),
            Err(PlanError::Gsi(IdPoolError::Exhausted {
                pool: "legacy GSI"
            }))
        );
        let full = ResourceBook {
            virtio_mmio: ResourceBook::fresh()
                .virtio_mmio
                .reserve(VIRTIO_MMIO_WINDOW)
                .unwrap(),
            legacy_gsi: ResourceBook::fresh().legacy_gsi,
        };
        assert_eq!(
            VirtioMmioGrant::allocate(&full).unwrap_err(),
            PlanError::Mmio(AddressPoolError::Exhausted {
                pool: "virtio-mmio"
            })
        );
        let configured = many(1);
        let narrow = HostCapabilities::fake(31, 8, 4096);
        assert!(matches!(
            MachinePlan::new(&configured, &narrow),
            Err(PlanError::BeyondMaxGpa { .. })
        ));
        assert!(matches!(
            plan(&configured).unwrap().replay(&narrow),
            Err(PlanError::BeyondMaxGpa { .. })
        ));
        assert!(MachinePlan::new(&spec([]), &narrow).is_ok());
    }

    #[test]
    fn replay_rejects_malformed_or_shared_grants_and_probe_order() {
        let original = plan(&spec([disk("a"), disk("b")])).unwrap();
        let grant = original.devices()["a"].block().unwrap().grant();
        let malformed = [
            Range::new(Gpa(0xc000_0001), 0x1000).unwrap(),
            Range::new(Gpa(0xc000_0000), 0x2000).unwrap(),
            Range::new(Gpa(0xc000_0000), 0).unwrap(),
            Range::new(Gpa(0xbfff_f000), 0x1000).unwrap(),
        ];
        for mmio in malformed {
            let mut bad = original.clone();
            let DevicePlan::Block(block) = bad.devices.get_mut("a").unwrap() else {
                panic!("expected block plan");
            };
            block.grant.mmio = mmio;
            assert!(bad.replay(&host()).is_err());
        }
        for shared in [
            grant,
            VirtioMmioGrant {
                mmio: Range::new(Gpa(0xc000_2000), 0x1000).unwrap(),
                ..grant
            },
            VirtioMmioGrant {
                gsi: Gsi(4),
                ..grant
            },
        ] {
            let mut bad = original.clone();
            let DevicePlan::Block(block) = bad.devices.get_mut("b").unwrap() else {
                panic!("expected block plan");
            };
            block.grant = shared;
            assert!(bad.replay(&host()).is_err());
        }
        for order in [vec!["a"], vec!["a", "a"], vec!["a", "missing"]] {
            let mut bad = original.clone();
            bad.probe_order = order.into_iter().map(Into::into).collect();
            assert_eq!(bad.replay(&host()), Err(PlanError::ProbeOrder));
        }
        let mut bad = original.clone();
        let DevicePlan::Block(block) = bad.devices.get_mut("b").unwrap() else {
            panic!("expected block plan");
        };
        block.root = true;
        assert_eq!(bad.replay(&host()), Err(PlanError::ProbeOrder));
        let DevicePlan::Block(block) = bad.devices.get_mut("a").unwrap() else {
            panic!("expected block plan");
        };
        block.root = true;
        assert_eq!(bad.replay(&host()), Err(PlanError::MultipleRoots));
        let configured = spec([disk("a")])
            .with_device_unchecked(DeviceSpec::Block(BlockSpec {
                id: "r1".into(),
                path: "/r1".into(),
                root: true,
                read_only: true,
            }))
            .with_device_unchecked(DeviceSpec::Block(BlockSpec {
                id: "r2".into(),
                path: "/r2".into(),
                root: true,
                read_only: true,
            }));
        assert_eq!(plan(&configured), Err(PlanError::MultipleRoots));
    }

    #[test]
    fn derived_cmdline_is_bounded_and_replay_never_appends() {
        let configured = spec([disk("a")]);
        let suffix_len =
            plan(&configured).unwrap().cmdline().as_str().len() - DEFAULT_CMDLINE.len();
        let longest = "x".repeat(CMDLINE_MAX_SIZE - 1 - suffix_len);
        let fits = configured
            .apply(SetKernel(kernel(Some(&longest))).into_vmm_command())
            .unwrap();
        let fits = plan(&fits).unwrap();
        assert_eq!(fits.cmdline().to_bytes_with_nul().len(), CMDLINE_MAX_SIZE);
        let before = fits.clone();
        fits.replay(&host()).unwrap();
        assert_eq!(fits, before);
        assert_eq!(fits.cmdline(), before.cmdline());
        let too_long = spec([
            disk("a"),
            SetKernel(kernel(Some(&(longest + "x")))).into_vmm_command(),
        ]);
        assert!(matches!(
            plan(&too_long),
            Err(PlanError::Cmdline(CmdlineError::TooLong {
                len: CMDLINE_MAX_SIZE,
                ..
            }))
        ));
        let empty = spec([disk("a"), SetKernel(kernel(Some(""))).into_vmm_command()]);
        assert_eq!(
            plan(&empty).unwrap().cmdline().as_str(),
            "virtio_mmio.device=4K@0xc0000000:5"
        );
    }

    #[test]
    fn hotplug_records_one_whole_object_and_preserves_boot_windows() {
        assert!(plan(&spec([])).unwrap().memory_hotplug().is_none());
        for (memory_mib, window_id, start) in [(128, 1, 4 << 30), (4096, 2, 5 << 30)] {
            let configured = spec([
                SetMemory(MemorySizeMib(memory_mib)).into_vmm_command(),
                memory_hotplug(256),
            ]);
            let (plan, policy) = MachinePlan::new(&configured, &host()).unwrap();
            let boot = MemoryLayout::boot(u64::from(memory_mib) << 20).unwrap();
            assert_eq!(
                plan.memory().boot_windows().collect::<Vec<_>>(),
                boot.boot_windows().collect::<Vec<_>>()
            );
            assert_eq!(plan.memory().objects().count(), boot.objects().count() + 1);
            assert_eq!(plan.memory().windows().count(), boot.windows().count() + 1);
            let mem = plan.memory_hotplug().unwrap();
            assert_eq!(mem.object_id(), ObjectId(1));
            assert_eq!(mem.window_id(), WindowId(window_id));
            let geometry = mem.geometry();
            assert_eq!(geometry.window().owner(), WindowOwner::VirtioMem);
            assert_eq!(geometry.window().kind(), WindowKind::VirtioMemRam);
            assert_eq!(
                geometry.region(),
                Range::new(Gpa(start), 256 << 20).unwrap()
            );
            assert_eq!(
                geometry.slice(),
                Range::new(ObjectOffset(0), 256 << 20).unwrap()
            );
            assert_eq!(geometry.block_size(), DEFAULT_BLOCK_SIZE);
            assert_eq!(geometry.chunk_size(), DEFAULT_CHUNK_SIZE);
            assert_eq!(geometry.chunk_count(), 2);
            assert_eq!(
                geometry,
                MemGeometry::new(
                    plan.memory(),
                    mem.window_id(),
                    DEFAULT_BLOCK_SIZE,
                    DEFAULT_CHUNK_SIZE,
                    policy.page_size,
                )
                .unwrap()
            );
            assert_eq!(
                MachinePlan::new(&configured, &host()).unwrap(),
                (plan.clone(), policy)
            );
            let shared = configured
                .apply(SetMemoryBacking(MemoryBacking::Shared).into_vmm_command())
                .unwrap();
            let (shared_plan, shared_policy) = MachinePlan::new(&shared, &host()).unwrap();
            assert_eq!(shared_plan, plan);
            assert_eq!(shared_policy.kind, BackingKind::SharedMemfd);
            assert!(plan.replay(&host()).is_ok());
        }
    }

    #[test]
    fn memory_is_probed_last_independent_of_configuration_order() {
        let root = AddDisk(BlockDevConfig {
            id: Some("root".into()),
            path: "/test/root".into(),
            read_only: true,
            root: true,
        });
        let baseline = plan(&spec([
            disk("z"),
            root.clone().into_vmm_command(),
            disk("a"),
            memory_hotplug(256),
        ]))
        .unwrap();
        for position in 0..=3 {
            let mut commands = vec![disk("z"), root.clone().into_vmm_command(), disk("a")];
            commands.insert(position, memory_hotplug(128));
            commands.push(memory_hotplug(256));
            let plan = plan(&spec(commands)).unwrap();
            assert_eq!(plan, baseline);
            assert_eq!(plan.probe_order(), ["root", "z", "a", VIRTIO_MEM_ID]);
            for (ordinal, id) in plan.probe_order().iter().enumerate() {
                let device = &plan.devices()[id];
                let grant = device.grant();
                assert_eq!(
                    grant.mmio.start(),
                    Gpa(0xc000_0000 + ordinal as u64 * 0x1000)
                );
                assert_eq!(grant.gsi, Gsi(5 + ordinal as u32));
                assert_eq!(device.block().is_some(), id != VIRTIO_MEM_ID);
                assert_eq!(device.mem().is_some(), id == VIRTIO_MEM_ID);
            }
            assert_eq!(
                plan.cmdline().as_str(),
                format!(
                    "{DEFAULT_CMDLINE} virtio_mmio.device=4K@0xc0000000:5 root=/dev/vda ro virtio_mmio.device=4K@0xc0001000:6 virtio_mmio.device=4K@0xc0002000:7 virtio_mmio.device=4K@0xc0003000:8"
                )
            );
        }
        let memory_only = plan(&spec([memory_hotplug(256)])).unwrap();
        assert_eq!(memory_only.probe_order(), [VIRTIO_MEM_ID]);
        assert_eq!(
            memory_only.cmdline().as_str(),
            format!("{DEFAULT_CMDLINE} virtio_mmio.device=4K@0xc0000000:5")
        );
    }

    #[test]
    fn all_chunk_permits_are_budgeted_on_new_and_replay() {
        for (memory_mib, chunk_size_mib, count) in [(128, 128, 3), (4096, 128, 4), (128, 64, 5)] {
            let configured = spec([
                SetMemory(MemorySizeMib(memory_mib)).into_vmm_command(),
                SetMemoryHotplug(MemoryHotplugConfig {
                    size_mib: 256,
                    block_size_mib: None,
                    chunk_size_mib: Some(chunk_size_mib),
                })
                .into_vmm_command(),
            ]);
            let exact = HostCapabilities::fake(46, count, 4096);
            let (plan, _) = MachinePlan::new(&configured, &exact).unwrap();
            assert!(plan.replay(&exact).is_ok());
            let short = HostCapabilities::fake(46, count - 1, 4096);
            let error = PlanError::Memslots {
                count,
                max: count - 1,
            };
            assert_eq!(MachinePlan::new(&configured, &short), Err(error.clone()));
            assert_eq!(plan.replay(&short), Err(error));
        }
    }

    #[test]
    fn malformed_unchecked_memory_geometry_returns_concrete_errors() {
        let valid = MemSpec {
            region_size: 256 << 20,
            block_size: DEFAULT_BLOCK_SIZE,
            chunk_size: DEFAULT_CHUNK_SIZE,
        };
        for (mem, error) in [
            (
                MemSpec {
                    block_size: 0,
                    ..valid.clone()
                },
                PlanError::Geometry(GeometryError::BlockSize(0)),
            ),
            (
                MemSpec {
                    block_size: 3,
                    ..valid.clone()
                },
                PlanError::Geometry(GeometryError::BlockSize(3)),
            ),
            (
                MemSpec {
                    chunk_size: 0,
                    ..valid.clone()
                },
                PlanError::Geometry(GeometryError::ChunkSize(0)),
            ),
            (
                MemSpec {
                    chunk_size: 3,
                    ..valid.clone()
                },
                PlanError::Geometry(GeometryError::ChunkSize(3)),
            ),
            (
                MemSpec {
                    chunk_size: 1 << 20,
                    ..valid.clone()
                },
                PlanError::Geometry(GeometryError::ChunkMultiple {
                    block: DEFAULT_BLOCK_SIZE,
                    chunk: 1 << 20,
                }),
            ),
            (
                MemSpec {
                    block_size: 2048,
                    ..valid.clone()
                },
                PlanError::Geometry(GeometryError::BelowBackingPage {
                    block: 2048,
                    page: 4096,
                }),
            ),
            (
                MemSpec {
                    region_size: 129 << 20,
                    ..valid.clone()
                },
                PlanError::Geometry(GeometryError::Region(WindowId(1))),
            ),
            (
                MemSpec {
                    region_size: 0,
                    ..valid.clone()
                },
                PlanError::DeviceMemory(AddressPoolError::InvalidRequest {
                    pool: "device memory",
                }),
            ),
            (
                MemSpec {
                    region_size: u64::MAX,
                    ..valid
                },
                PlanError::DeviceMemory(AddressPoolError::Exhausted {
                    pool: "device memory",
                }),
            ),
        ] {
            let configured = spec([]).with_device_unchecked(DeviceSpec::Mem(mem));
            assert_eq!(plan(&configured), Err(error));
        }
        let configured = spec([memory_hotplug(256)]);
        let large_page = HostCapabilities::fake(46, 8, 4 << 20);
        let error = PlanError::Geometry(GeometryError::BelowBackingPage {
            block: DEFAULT_BLOCK_SIZE,
            page: 4 << 20,
        });
        assert_eq!(
            MachinePlan::new(&configured, &large_page),
            Err(error.clone())
        );
        assert_eq!(plan(&configured).unwrap().replay(&large_page), Err(error));
    }

    #[test]
    fn stable_memory_identity_and_position_are_checked_on_new_and_replay() {
        let legacy = plan(&spec([disk(VIRTIO_MEM_ID)])).unwrap();
        assert!(legacy.memory_hotplug().is_none());
        assert!(legacy.replay(&host()).is_ok());
        let configured = spec([memory_hotplug(256)]);
        let collision = configured
            .clone()
            .with_device_unchecked(DeviceSpec::Block(BlockSpec {
                id: VIRTIO_MEM_ID.into(),
                path: "/test/disk".into(),
                read_only: false,
                root: false,
            }));
        assert_eq!(
            plan(&collision),
            Err(PlanError::DuplicateDevice(VIRTIO_MEM_ID.into()))
        );
        let duplicated = configured.with_device_unchecked(DeviceSpec::Mem(MemSpec {
            region_size: 256 << 20,
            block_size: DEFAULT_BLOCK_SIZE,
            chunk_size: DEFAULT_CHUNK_SIZE,
        }));
        assert_eq!(plan(&duplicated), Err(PlanError::MultipleMem));
        let original = plan(&spec([disk("data"), memory_hotplug(256)])).unwrap();
        let mut bad = original.clone();
        bad.devices
            .insert("extra".into(), original.devices()[VIRTIO_MEM_ID].clone());
        bad.probe_order.push("extra".into());
        assert_eq!(bad.replay(&host()), Err(PlanError::MultipleMem));
        let mut bad = original.clone();
        let mem = bad.devices.remove(VIRTIO_MEM_ID).unwrap();
        bad.devices.insert("other".into(), mem);
        bad.devices
            .insert(VIRTIO_MEM_ID.into(), original.devices()["data"].clone());
        bad.probe_order.push("other".into());
        assert_eq!(
            bad.replay(&host()),
            Err(PlanError::DuplicateDevice(VIRTIO_MEM_ID.into()))
        );
        let mut bad = original.clone();
        let mem = bad.devices.remove(VIRTIO_MEM_ID).unwrap();
        bad.devices.insert("other".into(), mem);
        *bad.probe_order.last_mut().unwrap() = "other".into();
        assert_eq!(bad.replay(&host()), Err(PlanError::MemId("other".into())));
        let mut bad = original;
        bad.probe_order.swap(0, 1);
        assert_eq!(bad.replay(&host()), Err(PlanError::ProbeOrder));
    }

    #[test]
    fn fresh_memory_ids_follow_existing_maxima_with_checked_increment() {
        let mem = MemSpec {
            region_size: 256 << 20,
            block_size: DEFAULT_BLOCK_SIZE,
            chunk_size: DEFAULT_CHUNK_SIZE,
        };
        let boot = |object_id, window_id| {
            MemoryLayout::new(
                [(
                    object_id,
                    Object {
                        capacity: 128 << 20,
                    },
                )],
                [(
                    window_id,
                    Window::new(
                        object_id,
                        Range::new(ObjectOffset(0), 128 << 20).unwrap(),
                        Gpa(0),
                        WindowOwner::Boot,
                        WindowKind::BootRam,
                    )
                    .unwrap(),
                )],
            )
            .unwrap()
        };
        let (_, geometry) =
            add_hotplug_memory(&boot(ObjectId(39), WindowId(71)), &mem, &host()).unwrap();
        assert_eq!(geometry.window().object(), ObjectId(40));
        assert_eq!(geometry.window_id(), WindowId(72));
        assert_eq!(
            add_hotplug_memory(&boot(ObjectId(u32::MAX), WindowId(0)), &mem, &host()),
            Err(PlanError::ObjectIdsExhausted)
        );
        assert_eq!(
            add_hotplug_memory(&boot(ObjectId(0), WindowId(u32::MAX)), &mem, &host()),
            Err(PlanError::WindowIdsExhausted)
        );
    }

    #[test]
    fn replay_keeps_explicit_ids_nonminimal_region_and_transport_grants() {
        let mut plan = plan(&spec([disk("data"), memory_hotplug(256)])).unwrap();
        let old = plan.memory_hotplug().unwrap().clone();
        let object_id = ObjectId(41);
        let window_id = WindowId(73);
        let window = Window::new(
            object_id,
            old.geometry().slice(),
            Gpa(8 << 30),
            WindowOwner::VirtioMem,
            WindowKind::VirtioMemRam,
        )
        .unwrap();
        plan.memory = MemoryLayout::new(
            plan.memory
                .objects()
                .map(|(id, object)| (if id == old.object_id() { object_id } else { id }, *object)),
            plan.memory.windows().map(|(id, old_window)| {
                if id == old.window_id() {
                    (window_id, window)
                } else {
                    (id, *old_window)
                }
            }),
        )
        .unwrap();
        let geometry = MemGeometry::new(
            &plan.memory,
            window_id,
            DEFAULT_BLOCK_SIZE,
            DEFAULT_CHUNK_SIZE,
            BackingPageSize::base(host().page_size),
        )
        .unwrap();
        let DevicePlan::Mem(mem) = plan.devices.get_mut(VIRTIO_MEM_ID).unwrap() else {
            panic!("expected memory plan");
        };
        mem.geometry = geometry;
        mem.grant = VirtioMmioGrant {
            mmio: Range::new(Gpa(0xc000_5000), VIRTIO_MMIO_LEN).unwrap(),
            gsi: Gsi(16),
        };
        let DevicePlan::Block(block) = plan.devices.get_mut("data").unwrap() else {
            panic!("expected block plan");
        };
        block.grant = VirtioMmioGrant {
            mmio: Range::new(Gpa(0xc000_4000), VIRTIO_MMIO_LEN).unwrap(),
            gsi: Gsi(15),
        };
        let before = plan.clone();
        let mut book = ResourceBook::fresh();
        for device in plan.devices().values() {
            book = device.grant().reserve(&book).unwrap();
        }
        for target in [host(), HostCapabilities::fake(34, 3, 4096)] {
            assert_eq!(plan.replay(&target), Ok(book.clone()));
            assert_eq!(plan, before);
            let mem = plan.memory_hotplug().unwrap();
            assert_eq!(mem.object_id(), object_id);
            assert_eq!(mem.window_id(), window_id);
            assert_eq!(mem.geometry().region().start(), Gpa(8 << 30));
        }
        assert_eq!(
            plan.cmdline().as_str(),
            format!(
                "{DEFAULT_CMDLINE} virtio_mmio.device=4K@0xc0004000:15 virtio_mmio.device=4K@0xc0005000:16"
            )
        );
    }

    #[test]
    fn replay_rejects_changed_windows_slices_objects_and_wrong_kinds() {
        let original = plan(&spec([memory_hotplug(256)])).unwrap();
        let geometry = original.memory_hotplug().unwrap().geometry();
        let make_window = |object, slice, start, owner, kind| {
            Window::new(object, slice, Gpa(start), owner, kind).unwrap()
        };
        let changed = [
            make_window(
                ObjectId(42),
                geometry.slice(),
                4 << 30,
                WindowOwner::VirtioMem,
                WindowKind::VirtioMemRam,
            ),
            make_window(
                ObjectId(1),
                geometry.slice(),
                5 << 30,
                WindowOwner::VirtioMem,
                WindowKind::VirtioMemRam,
            ),
            make_window(
                ObjectId(1),
                Range::new(ObjectOffset(0), 128 << 20).unwrap(),
                4 << 30,
                WindowOwner::VirtioMem,
                WindowKind::VirtioMemRam,
            ),
            make_window(
                ObjectId(1),
                Range::new(ObjectOffset(4096), 256 << 20).unwrap(),
                4 << 30,
                WindowOwner::VirtioMem,
                WindowKind::VirtioMemRam,
            ),
        ];
        for window in changed {
            let mut bad = original.clone();
            let capacity = window.slice().end().unwrap().0;
            replace_mem_window(&mut bad, window, capacity, false);
            assert_eq!(
                bad.replay(&host()),
                Err(PlanError::GeometryMismatch(WindowId(1)))
            );
        }
        for (owner, kind) in [
            (WindowOwner::Boot, WindowKind::VirtioMemRam),
            (WindowOwner::VirtioMem, WindowKind::BootRam),
            (WindowOwner::Boot, WindowKind::BootRam),
        ] {
            let mut bad = original.clone();
            let window = make_window(ObjectId(1), geometry.slice(), 4 << 30, owner, kind);
            replace_mem_window(&mut bad, window, geometry.region().len(), false);
            assert_eq!(
                bad.replay(&host()),
                Err(PlanError::Geometry(GeometryError::NotVirtioMem(WindowId(
                    1
                ))))
            );
        }
        // Even self-consistent saved geometry must be a whole, dedicated object.
        for (slice, capacity) in [
            (geometry.slice(), (256 << 20) + 4096),
            (
                Range::new(ObjectOffset(4096), 256 << 20).unwrap(),
                (256 << 20) + 4096,
            ),
        ] {
            let mut bad = original.clone();
            let window = make_window(
                ObjectId(1),
                slice,
                4 << 30,
                WindowOwner::VirtioMem,
                WindowKind::VirtioMemRam,
            );
            replace_mem_window(&mut bad, window, capacity, true);
            assert_eq!(
                bad.replay(&host()),
                Err(PlanError::GeometryMismatch(WindowId(1)))
            );
        }
        let mut bad = original;
        let window = make_window(
            ObjectId(1),
            geometry.slice(),
            (4 << 30) + DEFAULT_CHUNK_SIZE,
            WindowOwner::VirtioMem,
            WindowKind::VirtioMemRam,
        );
        replace_mem_window(&mut bad, window, geometry.region().len(), true);
        assert_eq!(
            bad.replay(&host()),
            Err(PlanError::MemRegionUnaligned(window.placement()))
        );
    }

    #[test]
    fn replay_rejects_unclaimed_or_missing_memory_resources() {
        let original = plan(&spec([memory_hotplug(256)])).unwrap();
        let mut missing_device = original.clone();
        missing_device.devices.clear();
        missing_device.probe_order.clear();
        assert_eq!(
            missing_device.replay(&host()),
            Err(PlanError::UnexpectedWindow(WindowId(1)))
        );
        let mut missing_window = original.clone();
        missing_window.memory = MemoryLayout::new(
            original.memory.objects().map(|(id, object)| (id, *object)),
            original
                .memory
                .boot_windows()
                .map(|(id, window)| (id, *window)),
        )
        .unwrap();
        assert_eq!(
            missing_window.replay(&host()),
            Err(PlanError::Geometry(GeometryError::UnknownWindow(WindowId(
                1
            ))))
        );
        for mut bad in [original.clone(), plan(&spec([])).unwrap()] {
            bad.memory = MemoryLayout::new(
                bad.memory
                    .objects()
                    .map(|(id, object)| (id, *object))
                    .chain([(
                        ObjectId(42),
                        Object {
                            capacity: 128 << 20,
                        },
                    )]),
                bad.memory.windows().map(|(id, window)| (id, *window)),
            )
            .unwrap();
            assert_eq!(
                bad.replay(&host()),
                Err(PlanError::UnexpectedObject(ObjectId(42)))
            );
        }
        for (owner, kind) in [
            (WindowOwner::VirtioMem, WindowKind::VirtioMemRam),
            (WindowOwner::VirtioMem, WindowKind::BootRam),
            (WindowOwner::Boot, WindowKind::VirtioMemRam),
        ] {
            let mut bad = original.clone();
            let extra = Window::new(
                ObjectId(42),
                Range::new(ObjectOffset(0), 128 << 20).unwrap(),
                Gpa(16 << 30),
                owner,
                kind,
            )
            .unwrap();
            bad.memory = MemoryLayout::new(
                bad.memory
                    .objects()
                    .map(|(id, object)| (id, *object))
                    .chain([(
                        ObjectId(42),
                        Object {
                            capacity: 128 << 20,
                        },
                    )]),
                bad.memory
                    .windows()
                    .map(|(id, window)| (id, *window))
                    .chain([(WindowId(77), extra)]),
            )
            .unwrap();
            assert_eq!(
                bad.replay(&host()),
                Err(PlanError::UnexpectedWindow(WindowId(77)))
            );
        }
    }

    #[test]
    fn host_and_arch_apertures_bound_new_regions_and_recorded_replay() {
        let exhausted = PlanError::DeviceMemory(AddressPoolError::Exhausted {
            pool: "device memory",
        });
        let configured = spec([memory_hotplug(256)]);
        assert_eq!(
            MachinePlan::new(&configured, &HostCapabilities::fake(32, 8, 4096)),
            Err(exhausted.clone())
        );
        assert_eq!(
            MachinePlan::new(&configured, &HostCapabilities::fake(31, 8, 4096)),
            Err(PlanError::DeviceMemoryAperture(LayoutError::Overflow))
        );
        let original = plan(&configured).unwrap();
        assert!(matches!(
            original.replay(&HostCapabilities::fake(32, 8, 4096)),
            Err(PlanError::Policy(HostPolicyError::BeyondMaxGpa { .. }))
        ));
        let narrow = HostCapabilities::fake(33, 64, 4096);
        let fits = spec([memory_hotplug(4096)]);
        let (exact, _) = MachinePlan::new(&fits, &narrow).unwrap();
        assert_eq!(
            exact.memory_hotplug().unwrap().geometry().region().last(),
            Some(narrow.max_gpa())
        );
        assert!(exact.replay(&narrow).is_ok());
        let too_large = spec([memory_hotplug(4096 + 128)]);
        assert_eq!(
            MachinePlan::new(&too_large, &narrow),
            Err(exhausted.clone())
        );
        assert!(matches!(
            plan(&too_large).unwrap().replay(&narrow),
            Err(PlanError::Policy(HostPolicyError::BeyondMaxGpa { .. }))
        ));
        let ceiling = plan(&spec([memory_hotplug(1 << 20)])).unwrap();
        assert_eq!(
            ceiling.memory_hotplug().unwrap().geometry().region().len(),
            1 << 40
        );
        assert!(ceiling.replay(&host()).is_ok());
        assert_eq!(
            plan(&spec([memory_hotplug((1 << 20) + 128)])),
            Err(exhausted.clone())
        );
        let after_high_ram = spec([
            SetMemory(MemorySizeMib(62 << 10)).into_vmm_command(),
            memory_hotplug(1024),
        ]);
        let limited = HostCapabilities::fake(36, 64, 4096);
        let (high, _) = MachinePlan::new(&after_high_ram, &limited).unwrap();
        assert_eq!(
            high.memory_hotplug().unwrap().geometry().region().start(),
            Gpa(63 << 30)
        );
        assert!(high.replay(&limited).is_ok());
        let too_large = after_high_ram.apply(memory_hotplug(1024 + 128)).unwrap();
        assert_eq!(MachinePlan::new(&too_large, &limited), Err(exhausted));
        let large_chunk = spec([
            SetMemory(MemorySizeMib(4096)).into_vmm_command(),
            SetMemoryHotplug(MemoryHotplugConfig {
                size_mib: 2048,
                block_size_mib: Some(2048),
                chunk_size_mib: Some(2048),
            })
            .into_vmm_command(),
        ]);
        let (aligned, _) = MachinePlan::new(&large_chunk, &narrow).unwrap();
        assert_eq!(
            aligned
                .memory_hotplug()
                .unwrap()
                .geometry()
                .region()
                .start(),
            Gpa(6 << 30)
        );
        assert!(aligned.replay(&narrow).is_ok());
        let geometry = original.memory_hotplug().unwrap().geometry();
        for start in [2 << 30, (4 << 30) + (1 << 40)] {
            let mut bad = original.clone();
            let window = Window::new(
                geometry.window().object(),
                geometry.slice(),
                Gpa(start),
                WindowOwner::VirtioMem,
                WindowKind::VirtioMemRam,
            )
            .unwrap();
            replace_mem_window(&mut bad, window, geometry.region().len(), true);
            assert!(matches!(
                bad.replay(&host()),
                Err(PlanError::DeviceMemory(
                    AddressPoolError::OutsideWindow { .. }
                ))
            ));
        }
    }

    #[test]
    fn memory_transport_grants_are_validated_like_disk_grants() {
        let original = plan(&spec([disk("data"), memory_hotplug(256)])).unwrap();
        let disk_grant = original.devices()["data"].grant();
        let mem_grant = original.devices()[VIRTIO_MEM_ID].grant();
        for grant in [
            disk_grant,
            VirtioMmioGrant {
                gsi: disk_grant.gsi,
                ..mem_grant
            },
            VirtioMmioGrant {
                gsi: Gsi(4),
                ..mem_grant
            },
            VirtioMmioGrant {
                mmio: Range::new(Gpa(0xc000_1001), VIRTIO_MMIO_LEN).unwrap(),
                ..mem_grant
            },
            VirtioMmioGrant {
                mmio: Range::new(Gpa(0xc000_1000), 0).unwrap(),
                ..mem_grant
            },
            VirtioMmioGrant {
                mmio: Range::new(Gpa(0xbfff_f000), VIRTIO_MMIO_LEN).unwrap(),
                ..mem_grant
            },
        ] {
            let mut bad = original.clone();
            let DevicePlan::Mem(mem) = bad.devices.get_mut(VIRTIO_MEM_ID).unwrap() else {
                panic!("expected memory plan");
            };
            mem.grant = grant;
            assert!(bad.replay(&host()).is_err());
        }
    }

    #[test]
    fn device_ids_must_be_unique() {
        // `apply` refuses duplicates, so only a spec made some other way has them.
        let duplicated = spec([disk("a")]).with_device_unchecked(DeviceSpec::Block(BlockSpec {
            id: "a".into(),
            path: "/test/other".into(),
            read_only: true,
            root: false,
        }));
        assert_eq!(
            plan(&duplicated),
            Err(PlanError::DuplicateDevice("a".into()))
        );
    }
}
