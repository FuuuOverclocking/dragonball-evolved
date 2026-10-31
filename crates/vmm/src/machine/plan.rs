//! The decisions that make a machine what the guest sees, taken once from its
//! spec and kept as a value.
//!
//! A plan is guest ABI only: how the host holds guest memory is a separate
//! [`HostPolicy`], decided alongside it but never part of it.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

use api::machine::ConsoleConfig;

use crate::align::BackingPageSize;
use crate::arch::x86_64::layout::CMDLINE_MAX_SIZE;
use crate::kvm_context::HostCapabilities;
use crate::memory::layout::{MemoryLayout, MemoryLayoutError};
use crate::memory::policy::{BackingKind, HostPolicy, HostPolicyError};
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
    /// disk {0} cannot be attached: virtio-blk is not supported yet
    DiskUnsupported(String),
}

/// What the guest sees of a machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachinePlan {
    vcpus: u8,
    memory: MemoryLayout,
    cmdline: KernelCmdline,
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
        let cmdline = KernelCmdline::new(kernel.cmdline.as_deref().unwrap_or(DEFAULT_CMDLINE))
            .map_err(PlanError::Cmdline)?;
        let mut ids = HashSet::new();
        if let Some(device) = spec
            .devices()
            .iter()
            .find(|device| !ids.insert(device.id()))
        {
            return Err(PlanError::DuplicateDevice(device.id().to_owned()));
        }
        // Refused rather than left out: a guest booted without the disks it
        // was configured with would run against the wrong root.
        if let Some(disk) = spec.blocks().next() {
            return Err(PlanError::DiskUnsupported(disk.id.clone()));
        }
        let plan = Self {
            vcpus,
            memory,
            cmdline,
            com1: spec.console() == ConsoleConfig::Stdio,
            kernel: kernel.path.clone(),
            initrd: kernel.initrd.clone(),
        };
        Ok((plan, policy))
    }

    pub fn vcpus(&self) -> u8 {
        self.vcpus
    }

    pub fn memory(&self) -> &MemoryLayout {
        &self.memory
    }

    pub fn cmdline(&self) -> &KernelCmdline {
        &self.cmdline
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
    fn disks_are_refused_until_virtio_blk_is_back() {
        let error = plan(&spec([disk("a"), disk("b")])).unwrap_err();
        assert_eq!(error, PlanError::DiskUnsupported("a".into()));
        assert_eq!(
            error.to_string(),
            "disk a cannot be attached: virtio-blk is not supported yet"
        );
    }

    #[test]
    fn device_ids_must_be_unique() {
        // Checked before disks are refused, so the rule holds for any device.
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
