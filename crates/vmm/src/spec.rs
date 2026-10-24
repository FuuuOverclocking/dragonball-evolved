use std::collections::HashSet;

use anyhow::{Result, bail};
use api::VmmCommand;
use api::block::{AddDisk, BlockDevConfig};
use api::boot::{KernelConfig, SetKernel};
use api::machine::{ConsoleConfig, MemorySizeMib, SetConsole, SetCpus, SetMemory, VcpuCount};

use crate::arch::x86_64::layout::ram_regions;
use crate::kvm_context::HostCapabilities;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineSpec {
    pub cpus: u8,
    pub memory_mib: u32,
    pub console: ConsoleConfig,
    pub kernel: Option<KernelConfig>,
    pub disks: Vec<BlockDevConfig>,
}

impl Default for MachineSpec {
    fn default() -> Self {
        Self {
            cpus: 1,
            memory_mib: 128,
            console: ConsoleConfig::Stdio,
            kernel: None,
            disks: Vec::new(),
        }
    }
}

impl MachineSpec {
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
            VmmCommand::SetConsole(SetConsole(value), ()) => next.console = value,
            VmmCommand::SetKernel(SetKernel(value), ()) => {
                if value.path.as_os_str().is_empty() {
                    bail!("kernel path must be nonempty");
                }
                next.kernel = Some(value);
            }
            VmmCommand::AddDisk(AddDisk(mut disk), ()) => {
                if disk.path.as_os_str().is_empty() {
                    bail!("disk path must be nonempty");
                }
                let id = disk
                    .id
                    .get_or_insert_with(|| format!("disk{}", next.disks.len()));
                if id.is_empty() || next.disks.iter().any(|disk| disk.id.as_ref() == Some(id)) {
                    bail!("disk id must be nonempty and unique");
                }
                if disk.root && next.disks.iter().any(|disk| disk.root) {
                    bail!("only one root disk is allowed");
                }
                next.disks.push(disk);
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
        if let Some(leaf) = host
            .supported_cpuid
            .as_slice()
            .iter()
            .find(|entry| entry.function == 0x8000_0008)
        {
            let bits = leaf.eax & 0xff;
            if bits < 64 && regions.last().unwrap().end() > (1u64 << bits) {
                bail!("guest memory exceeds supported physical address width");
            }
        }
        if self
            .disks
            .iter()
            .any(|disk| disk.id.as_deref().is_none_or(str::is_empty))
        {
            bail!("disk ids must be normalized before planning");
        }
        let mut ids = HashSet::new();
        if self.disks.iter().any(|disk| !ids.insert(disk.id.as_ref())) {
            bail!("duplicate disk ids");
        }
        if self.disks.iter().filter(|disk| disk.root).count() > 1 {
            bail!("multiple root disks");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use api::RawVmmOp;

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
        assert_eq!(spec.disks[0].id.as_deref(), Some("disk0"));
        assert!(spec.apply(AddDisk(disk).into_vmm_command()).is_err());
    }
}
