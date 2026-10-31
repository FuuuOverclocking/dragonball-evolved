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
    CMDLINE_MAX_SIZE, LEGACY_GSIS, VIRTIO_MMIO_LEN, VIRTIO_MMIO_WINDOW,
};
use crate::kvm_context::HostCapabilities;
use crate::memory::layout::{MemoryLayout, MemoryLayoutError};
use crate::memory::policy::{BackingKind, HostPolicy, HostPolicyError};
use crate::resources::{AddressPool, AddressPoolError, IdPool, IdPoolError};
use crate::space::{Gpa, Gsi, Range};
use crate::spec::MachineSpec;

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
    /// guest probe order must name every device exactly once, with the root first
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicePlan {
    Block(BlockPlan),
}

impl DevicePlan {
    pub fn block(&self) -> &BlockPlan {
        match self {
            Self::Block(block) => block,
        }
    }
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
        let memory =
            MemoryLayout::boot(u64::from(spec.memory_mib()) << 20).map_err(PlanError::Layout)?;
        let policy = HostPolicy {
            kind: BackingKind::from(spec.memory_backing()),
            page_size: BackingPageSize::base(host.page_size),
        };
        policy.check(&memory, host).map_err(PlanError::Policy)?;
        let base_cmdline = KernelCmdline::new(kernel.cmdline.as_deref().unwrap_or(DEFAULT_CMDLINE))
            .map_err(PlanError::Cmdline)?;
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
        let mut book = ResourceBook::fresh();
        let mut devices = BTreeMap::new();
        let mut probe_order = Vec::with_capacity(blocks.len());
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

    /// Root first, then the original configuration order, independent of map order.
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
            let block = self.devices[id].block();
            if !text.is_empty() {
                text.push(' ');
            }
            write!(
                text,
                "virtio_mmio.device=4K@{:#x}:{}",
                block.grant.mmio.start().0,
                block.grant.gsi.0
            )
            .unwrap();
            if block.root {
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
        let roots: Vec<_> = self
            .devices
            .iter()
            .filter(|(_, device)| device.block().root)
            .collect();
        if roots.len() > 1 {
            return Err(PlanError::MultipleRoots);
        }
        if let Some((id, _)) = roots.first()
            && self.probe_order.first() != Some(id)
        {
            return Err(PlanError::ProbeOrder);
        }
        let mut book = ResourceBook::fresh();
        for device in self.devices.values() {
            let grant = device.block().grant;
            book = grant.reserve(&book)?;
            if grant.mmio.last().unwrap() > host.max_gpa() {
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
        MemoryBacking, MemorySizeMib, SetConsole, SetCpus, SetMemory, SetMemoryBacking, VcpuCount,
    };

    use super::*;
    use crate::memory::layout::WindowId;
    use crate::space::Gpa;
    use crate::spec::{BlockSpec, DeviceSpec};

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
            let block = plan.devices()[id].block();
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
        let grant = original.devices()["a"].block().grant();
        let malformed = [
            Range::new(Gpa(0xc000_0001), 0x1000).unwrap(),
            Range::new(Gpa(0xc000_0000), 0x2000).unwrap(),
            Range::new(Gpa(0xc000_0000), 0).unwrap(),
            Range::new(Gpa(0xbfff_f000), 0x1000).unwrap(),
        ];
        for mmio in malformed {
            let mut bad = original.clone();
            let DevicePlan::Block(block) = bad.devices.get_mut("a").unwrap();
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
            let DevicePlan::Block(block) = bad.devices.get_mut("b").unwrap();
            block.grant = shared;
            assert!(bad.replay(&host()).is_err());
        }
        for order in [vec!["a"], vec!["a", "a"], vec!["a", "missing"]] {
            let mut bad = original.clone();
            bad.probe_order = order.into_iter().map(Into::into).collect();
            assert_eq!(bad.replay(&host()), Err(PlanError::ProbeOrder));
        }
        let mut bad = original.clone();
        let DevicePlan::Block(block) = bad.devices.get_mut("b").unwrap();
        block.root = true;
        assert_eq!(bad.replay(&host()), Err(PlanError::ProbeOrder));
        let DevicePlan::Block(block) = bad.devices.get_mut("a").unwrap();
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
