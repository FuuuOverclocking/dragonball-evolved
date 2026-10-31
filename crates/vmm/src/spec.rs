use std::path::PathBuf;

use api::VmmCommand;
use api::block::{AddDisk, BlockDevConfig};
use api::boot::{KernelConfig, SetKernel};
use api::machine::{
    ConsoleConfig, MemoryBacking, MemorySizeMib, SetConsole, SetCpus, SetMemory, SetMemoryBacking,
    VcpuCount,
};

// xAPIC IDs are 8 bits wide and 0xff is the broadcast ID.
const MAX_VCPUS: u8 = 254;

/// The machine as configured so far.
///
/// Only [`MachineSpec::apply`] produces a changed spec, so every value obeys
/// the configuration rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineSpec {
    cpus: u8,
    memory_mib: u32,
    memory_backing: MemoryBacking,
    console: ConsoleConfig,
    kernel: Option<KernelConfig>,
    devices: Vec<DeviceSpec>,
}

/// A configured device, classified by kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceSpec {
    Block(BlockSpec),
}

impl DeviceSpec {
    pub fn id(&self) -> &str {
        match self {
            Self::Block(block) => &block.id,
        }
    }
}

/// A disk as configured, with its id already normalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSpec {
    pub id: String,
    pub path: PathBuf,
    pub read_only: bool,
    pub root: bool,
}

#[derive(Debug, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum SpecError {
    /// vCPU count {0} is not between 1 and 254
    VcpuCount(u8),
    /// guest memory size must be non-zero
    ZeroMemory,
    /// kernel path must be non-empty
    EmptyKernelPath,
    /// disk path must be non-empty
    EmptyDiskPath,
    /// disk id must be non-empty
    EmptyDiskId,
    /// device id {0} is already in use
    DuplicateId(String),
    /// only one root disk is allowed
    SecondRootDisk,
    /// operation is not a machine configuration command
    NotConfiguration,
}

impl Default for MachineSpec {
    fn default() -> Self {
        Self {
            cpus: 1,
            memory_mib: 128,
            memory_backing: MemoryBacking::Anonymous,
            console: ConsoleConfig::Stdio,
            kernel: None,
            devices: Vec::new(),
        }
    }
}

impl MachineSpec {
    pub fn cpus(&self) -> u8 {
        self.cpus
    }

    pub fn memory_mib(&self) -> u32 {
        self.memory_mib
    }

    pub fn memory_backing(&self) -> MemoryBacking {
        self.memory_backing
    }

    pub fn console(&self) -> ConsoleConfig {
        self.console
    }

    pub fn kernel(&self) -> Option<&KernelConfig> {
        self.kernel.as_ref()
    }

    /// The configured devices, in configuration order.
    pub fn devices(&self) -> &[DeviceSpec] {
        &self.devices
    }

    /// The configured disks, in configuration order.
    pub fn blocks(&self) -> impl Iterator<Item = &BlockSpec> {
        self.devices.iter().map(|device| match device {
            DeviceSpec::Block(block) => block,
        })
    }

    pub fn apply(&self, command: VmmCommand) -> Result<Self, SpecError> {
        let mut next = self.clone();
        match command {
            VmmCommand::SetCpus(SetCpus(VcpuCount(count)), ()) => {
                if !(1..=MAX_VCPUS).contains(&count) {
                    return Err(SpecError::VcpuCount(count));
                }
                next.cpus = count;
            }
            VmmCommand::SetMemory(SetMemory(MemorySizeMib(size)), ()) => {
                if size == 0 {
                    return Err(SpecError::ZeroMemory);
                }
                next.memory_mib = size;
            }
            VmmCommand::SetMemoryBacking(SetMemoryBacking(backing), ()) => {
                next.memory_backing = backing;
            }
            VmmCommand::SetConsole(SetConsole(console), ()) => next.console = console,
            VmmCommand::SetKernel(SetKernel(kernel), ()) => {
                if kernel.path.as_os_str().is_empty() {
                    return Err(SpecError::EmptyKernelPath);
                }
                next.kernel = Some(kernel);
            }
            VmmCommand::AddDisk(AddDisk(disk), ()) => {
                let block = self.block(disk)?;
                next.devices.push(DeviceSpec::Block(block));
            }
            VmmCommand::GetInstanceInfo(..)
            | VmmCommand::UpdateLogger(..)
            | VmmCommand::Start(..)
            | VmmCommand::Pause(..)
            | VmmCommand::Resume(..)
            | VmmCommand::Shutdown(..)
            | VmmCommand::HandoverState(..) => return Err(SpecError::NotConfiguration),
        }
        Ok(next)
    }

    fn block(&self, disk: BlockDevConfig) -> Result<BlockSpec, SpecError> {
        if disk.path.as_os_str().is_empty() {
            return Err(SpecError::EmptyDiskPath);
        }
        let id = disk
            .id
            .unwrap_or_else(|| format!("disk{}", self.blocks().count()));
        if id.is_empty() {
            return Err(SpecError::EmptyDiskId);
        }
        if self.devices.iter().any(|device| device.id() == id) {
            return Err(SpecError::DuplicateId(id));
        }
        if disk.root && self.blocks().any(|block| block.root) {
            return Err(SpecError::SecondRootDisk);
        }
        Ok(BlockSpec {
            id,
            path: disk.path,
            read_only: disk.read_only,
            root: disk.root,
        })
    }
}

#[cfg(test)]
mod tests {
    use api::RawVmmOp;
    use api::lifecycle::Start;

    use super::*;

    fn disk(id: Option<&str>, root: bool) -> AddDisk {
        AddDisk(BlockDevConfig {
            id: id.map(Into::into),
            path: "/test/disk".into(),
            read_only: false,
            root,
        })
    }

    #[test]
    fn invalid_updates_leave_the_original_unchanged() {
        let spec = MachineSpec::default();
        assert_eq!(
            spec.apply(SetCpus(VcpuCount(0)).into_vmm_command()),
            Err(SpecError::VcpuCount(0))
        );
        assert_eq!(
            spec.apply(SetCpus(VcpuCount(255)).into_vmm_command()),
            Err(SpecError::VcpuCount(255))
        );
        assert_eq!(
            spec.apply(SetMemory(MemorySizeMib(0)).into_vmm_command()),
            Err(SpecError::ZeroMemory)
        );
        assert_eq!(spec, MachineSpec::default());
        for count in [1, 2, MAX_VCPUS] {
            let next = spec
                .apply(SetCpus(VcpuCount(count)).into_vmm_command())
                .unwrap();
            assert_eq!(next.cpus(), count);
        }
    }

    #[test]
    fn kernel_and_disk_paths_must_be_non_empty() {
        let spec = MachineSpec::default();
        let kernel = KernelConfig {
            path: PathBuf::new(),
            cmdline: None,
            initrd: None,
        };
        assert_eq!(
            spec.apply(SetKernel(kernel.clone()).into_vmm_command()),
            Err(SpecError::EmptyKernelPath)
        );
        let kernel = KernelConfig {
            path: "/test/kernel".into(),
            ..kernel
        };
        let next = spec
            .apply(SetKernel(kernel.clone()).into_vmm_command())
            .unwrap();
        assert_eq!(next.kernel(), Some(&kernel));

        let mut disk = disk(None, false);
        disk.0.path = PathBuf::new();
        assert_eq!(
            spec.apply(disk.into_vmm_command()),
            Err(SpecError::EmptyDiskPath)
        );
    }

    #[test]
    fn ids_and_root_disks_are_unique() {
        let spec = MachineSpec::default()
            .apply(disk(None, true).into_vmm_command())
            .unwrap();
        assert_eq!(
            spec.devices(),
            [DeviceSpec::Block(BlockSpec {
                id: "disk0".into(),
                path: "/test/disk".into(),
                read_only: false,
                root: true,
            })]
        );
        assert_eq!(
            spec.apply(disk(None, true).into_vmm_command()),
            Err(SpecError::SecondRootDisk)
        );
        assert_eq!(
            spec.apply(disk(Some("disk0"), false).into_vmm_command()),
            Err(SpecError::DuplicateId("disk0".into()))
        );
        assert_eq!(
            spec.apply(disk(Some(""), false).into_vmm_command()),
            Err(SpecError::EmptyDiskId)
        );

        let spec = spec
            .apply(disk(Some("data"), false).into_vmm_command())
            .unwrap()
            .apply(disk(None, false).into_vmm_command())
            .unwrap();
        let ids: Vec<_> = spec.blocks().map(|block| block.id.as_str()).collect();
        assert_eq!(ids, ["disk0", "data", "disk2"]);
    }

    #[test]
    fn non_configuration_commands_are_rejected() {
        assert_eq!(
            MachineSpec::default().apply(Start.into_vmm_command()),
            Err(SpecError::NotConfiguration)
        );
    }
}
