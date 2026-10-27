use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Result, bail};
use api::VmmCommand;
use api::block::AddDisk;
use api::boot::{KernelConfig, SetKernel};
use api::machine::{
    ConsoleConfig, MemoryBacking, MemorySizeMib, SetConsole, SetCpus, SetMemory, SetMemoryBacking,
    VcpuCount,
};

use crate::arch::x86_64::layout::ram_regions;
use crate::kvm_context::HostCapabilities;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineSpec {
    pub cpus: u8,
    pub memory_mib: u32,
    pub memory_backing: MemoryBacking,
    pub console: ConsoleConfig,
    pub kernel: Option<KernelConfig>,
    pub devices: Vec<DeviceSpec>,
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
    /// The configured disks, in configuration order.
    pub fn blocks(&self) -> impl Iterator<Item = &BlockSpec> {
        self.devices.iter().map(|device| match device {
            DeviceSpec::Block(block) => block,
        })
    }

    pub fn apply(&self, command: VmmCommand) -> Result<Self> {
        let mut next = self.clone();
        match command {
            VmmCommand::SetCpus(SetCpus(VcpuCount(count)), ()) => {
                if !(1..=254).contains(&count) {
                    bail!("vCPU count must be between 1 and 254");
                }
                next.cpus = count;
            }
            VmmCommand::SetMemory(SetMemory(MemorySizeMib(size)), ()) => {
                if size == 0 {
                    bail!("guest memory size must be non-zero");
                }
                next.memory_mib = size;
            }
            VmmCommand::SetMemoryBacking(SetMemoryBacking(value), ()) => {
                next.memory_backing = value;
            }
            VmmCommand::SetConsole(SetConsole(value), ()) => next.console = value,
            VmmCommand::SetKernel(SetKernel(value), ()) => {
                if value.path.as_os_str().is_empty() {
                    bail!("kernel path must be nonempty");
                }
                next.kernel = Some(value);
            }
            VmmCommand::AddDisk(AddDisk(disk), ()) => {
                if disk.path.as_os_str().is_empty() {
                    bail!("disk path must be nonempty");
                }
                let id = disk
                    .id
                    .unwrap_or_else(|| format!("disk{}", next.blocks().count()));
                if id.is_empty() || next.devices.iter().any(|device| device.id() == id) {
                    bail!("disk id must be nonempty and unique");
                }
                if disk.root && next.blocks().any(|block| block.root) {
                    bail!("only one root disk is allowed");
                }
                next.devices.push(DeviceSpec::Block(BlockSpec {
                    id,
                    path: disk.path,
                    read_only: disk.read_only,
                    root: disk.root,
                }));
            }
            _ => bail!("operation is not a machine configuration command"),
        }
        Ok(next)
    }

    pub fn validate(&self, host: &HostCapabilities) -> Result<()> {
        if self.cpus == 0 || self.cpus > 254 || usize::from(self.cpus) > host.max_vcpus {
            bail!("vCPU count exceeds host or xAPIC limits");
        }
        if self.kernel.is_none() {
            bail!("kernel is not configured");
        }
        let regions = ram_regions(u64::from(self.memory_mib) << 20)?;
        if self.memory_mib < 16 {
            bail!("PVH boot requires at least 16 MiB of memory");
        }
        if regions.len() > host.max_memslots {
            bail!("guest memory exceeds KVM memslot limit");
        }
        if regions.last().unwrap().end() > host.physical_address_limit() {
            bail!("guest memory exceeds supported physical address width");
        }
        let mut ids = HashSet::new();
        if !self.devices.iter().all(|device| ids.insert(device.id())) {
            bail!("duplicate device ids");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use api::RawVmmOp;
    use api::block::BlockDevConfig;

    use super::*;

    #[test]
    fn invalid_updates_leave_the_original_unchanged() {
        let spec = MachineSpec::default();
        assert!(
            spec.apply(SetCpus(VcpuCount(0)).into_vmm_command())
                .is_err()
        );
        assert!(
            spec.apply(SetMemory(MemorySizeMib(0)).into_vmm_command())
                .is_err()
        );
        assert_eq!(spec, MachineSpec::default());
        assert_eq!(
            spec.apply(SetCpus(VcpuCount(2)).into_vmm_command())
                .unwrap()
                .cpus,
            2
        );
    }

    #[test]
    fn ids_and_root_disks_are_unique() {
        let disk = BlockDevConfig {
            id: None,
            path: "/test/disk".into(),
            read_only: false,
            root: true,
        };
        let spec = MachineSpec::default()
            .apply(AddDisk(disk.clone()).into_vmm_command())
            .unwrap();
        assert_eq!(
            spec.devices,
            [DeviceSpec::Block(BlockSpec {
                id: "disk0".into(),
                path: "/test/disk".into(),
                read_only: false,
                root: true,
            })]
        );
        assert!(spec.apply(AddDisk(disk).into_vmm_command()).is_err());
    }
}
