use std::path::PathBuf;

use api::VmmCommand;
use api::block::{AddDisk, BlockDevConfig};
use api::boot::{KernelConfig, SetKernel};
use api::machine::{
    ConsoleConfig, MemoryBacking, MemoryHotplugConfig, MemorySizeMib, SetConsole, SetCpus,
    SetMemory, SetMemoryBacking, SetMemoryHotplug, VcpuCount,
};

use crate::memory::hotplug::{DEFAULT_BLOCK_SIZE, DEFAULT_CHUNK_SIZE};

// xAPIC IDs are 8 bits wide and 0xff is the broadcast ID.
const MAX_VCPUS: u8 = 254;

pub const VIRTIO_MEM_ID: &str = "virtio-mem0";

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
    Mem(MemSpec),
}

impl DeviceSpec {
    pub fn id(&self) -> &str {
        match self {
            Self::Block(block) => &block.id,
            Self::Mem(_) => VIRTIO_MEM_ID,
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

/// Memory hotplug geometry, normalized to bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemSpec {
    pub region_size: u64,
    pub block_size: u64,
    pub chunk_size: u64,
}

#[derive(Debug, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum SpecError {
    /// vCPU count {0} is not between 1 and 254
    VcpuCount(u8),
    /// guest memory size must be non-zero
    ZeroMemory,
    /// hotplug memory size must be non-zero
    ZeroHotplugMemory,
    /// hotplug block size {0} bytes is not a non-zero power of two
    HotplugBlockSize(u64),
    /// hotplug chunk size {0} bytes is not a non-zero power of two
    HotplugChunkSize(u64),
    /// hotplug chunk size {chunk_size} bytes must be at least and a multiple of block size {block_size} bytes
    HotplugChunkAlignment { chunk_size: u64, block_size: u64 },
    /// hotplug region size {region_size} bytes is not a multiple of chunk size {chunk_size} bytes
    HotplugRegionAlignment { region_size: u64, chunk_size: u64 },
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

impl MemSpec {
    fn new(config: MemoryHotplugConfig) -> Result<Self, SpecError> {
        let bytes = |size_mib: u32| u64::from(size_mib) << 20;
        let region_size = bytes(config.size_mib);
        let block_size = config
            .block_size_mib
            .map(bytes)
            .unwrap_or(DEFAULT_BLOCK_SIZE);
        let chunk_size = config
            .chunk_size_mib
            .map(bytes)
            .unwrap_or(DEFAULT_CHUNK_SIZE);
        if region_size == 0 {
            return Err(SpecError::ZeroHotplugMemory);
        }
        if !block_size.is_power_of_two() {
            return Err(SpecError::HotplugBlockSize(block_size));
        }
        if !chunk_size.is_power_of_two() {
            return Err(SpecError::HotplugChunkSize(chunk_size));
        }
        if chunk_size < block_size || chunk_size % block_size != 0 {
            return Err(SpecError::HotplugChunkAlignment {
                chunk_size,
                block_size,
            });
        }
        if region_size % chunk_size != 0 {
            return Err(SpecError::HotplugRegionAlignment {
                region_size,
                chunk_size,
            });
        }
        Ok(Self {
            region_size,
            block_size,
            chunk_size,
        })
    }
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
        self.devices.iter().filter_map(|device| match device {
            DeviceSpec::Block(block) => Some(block),
            DeviceSpec::Mem(_) => None,
        })
    }

    pub fn memory_hotplug(&self) -> Option<&MemSpec> {
        self.devices.iter().find_map(|device| match device {
            DeviceSpec::Block(_) => None,
            DeviceSpec::Mem(mem) => Some(mem),
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
            VmmCommand::SetMemoryHotplug(SetMemoryHotplug(config), ()) => {
                let mem = MemSpec::new(config)?;
                if self.blocks().any(|block| block.id == VIRTIO_MEM_ID) {
                    return Err(SpecError::DuplicateId(VIRTIO_MEM_ID.into()));
                }
                if let Some(device) = next
                    .devices
                    .iter_mut()
                    .find(|device| matches!(device, DeviceSpec::Mem(_)))
                {
                    *device = DeviceSpec::Mem(mem);
                } else {
                    next.devices.push(DeviceSpec::Mem(mem));
                }
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
            | VmmCommand::ResizeMemory(..)
            | VmmCommand::Start(..)
            | VmmCommand::Pause(..)
            | VmmCommand::Resume(..)
            | VmmCommand::Shutdown(..)
            | VmmCommand::HandoverState(..) => return Err(SpecError::NotConfiguration),
        }
        Ok(next)
    }

    /// Adds `device` without the rules `apply` enforces.
    #[cfg(test)]
    pub(crate) fn with_device_unchecked(mut self, device: DeviceSpec) -> Self {
        self.devices.push(device);
        self
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
    use api::machine::{HotplugSizeMib, ResizeMemory};

    use super::*;

    fn disk(id: Option<&str>, root: bool) -> AddDisk {
        AddDisk(BlockDevConfig {
            id: id.map(Into::into),
            path: "/test/disk".into(),
            read_only: false,
            root,
        })
    }

    fn memory_hotplug(size_mib: u32) -> SetMemoryHotplug {
        SetMemoryHotplug(MemoryHotplugConfig {
            size_mib,
            block_size_mib: None,
            chunk_size_mib: None,
        })
    }

    #[test]
    fn memory_hotplug_defaults_are_normalized_to_bytes() {
        let spec = MachineSpec::default();
        assert_eq!(spec.memory_hotplug(), None);
        let next = spec.apply(memory_hotplug(256).into_vmm_command()).unwrap();
        let mem = MemSpec {
            region_size: 256 << 20,
            block_size: DEFAULT_BLOCK_SIZE,
            chunk_size: DEFAULT_CHUNK_SIZE,
        };
        assert_eq!(next.memory_hotplug(), Some(&mem));
        assert_eq!(next.devices(), [DeviceSpec::Mem(mem)]);
        assert_eq!(next.devices()[0].id(), VIRTIO_MEM_ID);
        assert_eq!(next.blocks().count(), 0);
        assert_eq!(next.memory_mib(), spec.memory_mib());
        assert_eq!(spec, MachineSpec::default());
    }

    #[test]
    fn memory_hotplug_optional_geometry_and_large_sizes_are_normalized() {
        for (block_size_mib, chunk_size_mib, block_size, chunk_size) in [
            (Some(4), None, 4 << 20, DEFAULT_CHUNK_SIZE),
            (None, Some(64), DEFAULT_BLOCK_SIZE, 64 << 20),
            (Some(4), Some(64), 4 << 20, 64 << 20),
        ] {
            let spec = MachineSpec::default()
                .apply(
                    SetMemoryHotplug(MemoryHotplugConfig {
                        size_mib: 256,
                        block_size_mib,
                        chunk_size_mib,
                    })
                    .into_vmm_command(),
                )
                .unwrap();
            assert_eq!(
                spec.memory_hotplug(),
                Some(&MemSpec {
                    region_size: 256 << 20,
                    block_size,
                    chunk_size,
                })
            );
        }
        for (size_mib, block_size_mib, chunk_size_mib) in
            [(u32::MAX, 1, 1), (1 << 31, 1 << 31, 1 << 31)]
        {
            let spec = MachineSpec::default()
                .apply(
                    SetMemoryHotplug(MemoryHotplugConfig {
                        size_mib,
                        block_size_mib: Some(block_size_mib),
                        chunk_size_mib: Some(chunk_size_mib),
                    })
                    .into_vmm_command(),
                )
                .unwrap();
            assert_eq!(
                spec.memory_hotplug(),
                Some(&MemSpec {
                    region_size: u64::from(size_mib) << 20,
                    block_size: u64::from(block_size_mib) << 20,
                    chunk_size: u64::from(chunk_size_mib) << 20,
                })
            );
        }
    }

    #[test]
    fn invalid_memory_hotplug_geometry_leaves_the_original_unchanged() {
        let spec = MachineSpec::default()
            .apply(memory_hotplug(256).into_vmm_command())
            .unwrap();
        let original = spec.clone();
        let config = memory_hotplug(256).0;
        for (config, error) in [
            (
                MemoryHotplugConfig {
                    size_mib: 0,
                    ..config
                },
                SpecError::ZeroHotplugMemory,
            ),
            (
                MemoryHotplugConfig {
                    block_size_mib: Some(0),
                    ..config
                },
                SpecError::HotplugBlockSize(0),
            ),
            (
                MemoryHotplugConfig {
                    block_size_mib: Some(3),
                    ..config
                },
                SpecError::HotplugBlockSize(3 << 20),
            ),
            (
                MemoryHotplugConfig {
                    chunk_size_mib: Some(0),
                    ..config
                },
                SpecError::HotplugChunkSize(0),
            ),
            (
                MemoryHotplugConfig {
                    chunk_size_mib: Some(3),
                    ..config
                },
                SpecError::HotplugChunkSize(3 << 20),
            ),
            (
                MemoryHotplugConfig {
                    block_size_mib: Some(128),
                    chunk_size_mib: Some(64),
                    ..config
                },
                SpecError::HotplugChunkAlignment {
                    chunk_size: 64 << 20,
                    block_size: 128 << 20,
                },
            ),
            (
                MemoryHotplugConfig {
                    size_mib: 1,
                    ..config
                },
                SpecError::HotplugRegionAlignment {
                    region_size: 1 << 20,
                    chunk_size: DEFAULT_CHUNK_SIZE,
                },
            ),
            (
                MemoryHotplugConfig {
                    size_mib: 129,
                    ..config
                },
                SpecError::HotplugRegionAlignment {
                    region_size: 129 << 20,
                    chunk_size: DEFAULT_CHUNK_SIZE,
                },
            ),
        ] {
            assert_eq!(
                spec.apply(SetMemoryHotplug(config).into_vmm_command()),
                Err(error)
            );
            assert_eq!(spec, original);
        }
    }

    #[test]
    fn replacing_memory_hotplug_preserves_device_order_and_disk_numbering() {
        let spec = MachineSpec::default()
            .apply(disk(None, true).into_vmm_command())
            .unwrap()
            .apply(memory_hotplug(256).into_vmm_command())
            .unwrap()
            .apply(disk(None, false).into_vmm_command())
            .unwrap();
        let next = spec
            .apply(
                SetMemoryHotplug(MemoryHotplugConfig {
                    size_mib: 512,
                    block_size_mib: Some(4),
                    chunk_size_mib: Some(64),
                })
                .into_vmm_command(),
            )
            .unwrap();
        let ids: Vec<_> = next.devices().iter().map(DeviceSpec::id).collect();
        assert_eq!(ids, ["disk0", VIRTIO_MEM_ID, "disk1"]);
        assert_eq!(next.devices()[0], spec.devices()[0]);
        assert_eq!(next.devices()[2], spec.devices()[2]);
        assert_eq!(
            next.devices()
                .iter()
                .filter(|device| matches!(device, DeviceSpec::Mem(_)))
                .count(),
            1
        );
        assert_eq!(
            next.memory_hotplug(),
            Some(&MemSpec {
                region_size: 512 << 20,
                block_size: 4 << 20,
                chunk_size: 64 << 20,
            })
        );
        assert_eq!(spec.memory_hotplug().unwrap().region_size, 256 << 20);
        let next = next.apply(disk(None, false).into_vmm_command()).unwrap();
        let ids: Vec<_> = next.blocks().map(|block| block.id.as_str()).collect();
        assert_eq!(ids, ["disk0", "disk1", "disk2"]);
    }

    #[test]
    fn memory_hotplug_id_collides_with_disks_in_both_configuration_orders() {
        let spec = MachineSpec::default()
            .apply(disk(Some("data"), false).into_vmm_command())
            .unwrap()
            .apply(disk(Some(VIRTIO_MEM_ID), false).into_vmm_command())
            .unwrap();
        let original = spec.clone();
        assert_eq!(
            spec.apply(memory_hotplug(256).into_vmm_command()),
            Err(SpecError::DuplicateId(VIRTIO_MEM_ID.into()))
        );
        assert_eq!(spec, original);

        let spec = MachineSpec::default()
            .apply(memory_hotplug(256).into_vmm_command())
            .unwrap();
        let original = spec.clone();
        assert_eq!(
            spec.apply(disk(Some(VIRTIO_MEM_ID), false).into_vmm_command()),
            Err(SpecError::DuplicateId(VIRTIO_MEM_ID.into()))
        );
        assert_eq!(spec, original);
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
        let spec = MachineSpec::default();
        for command in [
            Start.into_vmm_command(),
            ResizeMemory(HotplugSizeMib(0)).into_vmm_command(),
            ResizeMemory(HotplugSizeMib(128)).into_vmm_command(),
        ] {
            assert_eq!(spec.apply(command), Err(SpecError::NotConfiguration));
        }
        assert_eq!(spec, MachineSpec::default());
    }
}
