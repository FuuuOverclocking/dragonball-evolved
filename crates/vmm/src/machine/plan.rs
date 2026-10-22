use anyhow::{Context, Result};
use linux_loader::cmdline::Cmdline;
use vm_memory::GuestAddress;

use crate::arch::x86_64::layout::{
    CMDLINE_MAX_SIZE, LEGACY_GSI_START, MemoryRegion, VIRTIO_MMIO_LEN, VIRTIO_MMIO_START,
    ram_regions,
};
use crate::kvm_context::HostCapabilities;
use crate::spec::MachineSpec;

pub const DEFAULT_CMDLINE: &str =
    "console=ttyS0 reboot=k panic=1 pci=off i8042.noaux i8042.nomux i8042.dumbkbd";

pub struct DeviceLocation {
    pub address: u64,
    pub gsi: u32,
}

pub struct MachinePlan {
    pub spec: MachineSpec,
    pub ram: Vec<MemoryRegion>,
    pub devices: Vec<DeviceLocation>,
    pub cmdline: Cmdline,
}

impl MachinePlan {
    pub fn new(mut spec: MachineSpec, host: &HostCapabilities) -> Result<Self> {
        spec.validate(host)?;
        spec.disks.sort_by_key(|disk| !disk.root);
        let ram = ram_regions(u64::from(spec.memory_mib) << 20)?;
        let mut cmdline = Cmdline::new(CMDLINE_MAX_SIZE)?;
        cmdline
            .insert_str(
                spec.kernel
                    .as_ref()
                    .unwrap()
                    .cmdline
                    .as_deref()
                    .unwrap_or(DEFAULT_CMDLINE),
            )
            .context("build kernel command line")?;
        let mut devices = Vec::with_capacity(spec.disks.len());
        for (index, disk) in spec.disks.iter().enumerate() {
            let location = DeviceLocation {
                address: VIRTIO_MMIO_START + index as u64 * VIRTIO_MMIO_LEN,
                gsi: LEGACY_GSI_START + index as u32,
            };
            cmdline.add_virtio_mmio_device(
                VIRTIO_MMIO_LEN,
                GuestAddress(location.address),
                location.gsi,
                None,
            )?;
            if disk.root {
                cmdline.insert("root", "/dev/vda")?;
                cmdline.insert_str(if disk.read_only { "ro" } else { "rw" })?;
            }
            devices.push(location);
        }
        Ok(Self {
            spec,
            ram,
            devices,
            cmdline,
        })
    }
}

#[cfg(test)]
mod tests {
    use api::block::BlockDevConfig;
    use api::boot::KernelConfig;
    use kvm_bindings::CpuId;

    use super::*;
    use crate::arch::x86_64::cpuid::{CpuVendor, HostCpuid};

    fn host() -> HostCapabilities {
        HostCapabilities {
            supported_cpuid: CpuId::new(0).unwrap(),
            host_cpuid: HostCpuid {
                vendor: CpuVendor::Amd,
                extended_l1: Default::default(),
                extended_l2: Default::default(),
                amd_cache_topology: Vec::new(),
                amd_processor_topology: Default::default(),
            },
            max_vcpus: 254,
            max_memslots: 2,
        }
    }

    fn fixture() -> MachineSpec {
        MachineSpec {
            kernel: Some(KernelConfig {
                path: "/kernel".into(),
                cmdline: None,
                initrd: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn root_disk_is_first_and_cmdline_matches_the_resource_layout() {
        let mut spec = fixture();
        spec.memory_mib = 4096;
        spec.disks = vec![
            BlockDevConfig {
                id: Some("data".into()),
                path: "/data".into(),
                read_only: false,
                root: false,
            },
            BlockDevConfig {
                id: Some("root".into()),
                path: "/root".into(),
                read_only: true,
                root: true,
            },
        ];
        let plan = MachinePlan::new(spec, &host()).unwrap();
        assert_eq!(plan.spec.disks[0].id.as_deref(), Some("root"));
        assert_eq!(plan.devices[0].address, VIRTIO_MMIO_START);
        assert_eq!(plan.devices[1].gsi, LEGACY_GSI_START + 1);
        assert_eq!(plan.ram.len(), 2);
        let cmdline = plan.cmdline.as_cstring().unwrap();
        let cmdline = cmdline.to_str().unwrap();
        assert!(cmdline.contains("virtio_mmio.device=4K@0xc0000000:5"));
        assert!(cmdline.contains("root=/dev/vda ro"));
    }

    #[test]
    fn invalid_configurations_are_rejected_without_opening_kvm_or_files() {
        let mut spec = fixture();
        spec.cpus = 255;
        assert!(MachinePlan::new(spec, &host()).is_err());
        let mut spec = fixture();
        spec.kernel.as_mut().unwrap().cmdline = Some("x".repeat(CMDLINE_MAX_SIZE + 1));
        assert!(MachinePlan::new(spec, &host()).is_err());
    }
}
