use anyhow::{Context, Result};
use linux_loader::cmdline::Cmdline;
use vm_memory::GuestAddress;

use crate::arch::x86_64::layout::{
    CMDLINE_MAX_SIZE, MemoryRegion, VIRTIO_MMIO_LEN, ram_regions, resource_book,
};
use crate::kvm_context::HostCapabilities;
use crate::resources::{GpaRange, Gsi, ResourceBook, ResourceError};
use crate::spec::MachineSpec;

pub const DEFAULT_CMDLINE: &str =
    "console=ttyS0 reboot=k panic=1 pci=off i8042.noaux i8042.nomux i8042.dumbkbd";

/// The register window and interrupt line granted to one virtio-mmio device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtioMmioGrant {
    pub mmio: GpaRange,
    pub gsi: Gsi,
}

impl VirtioMmioGrant {
    fn allocate(book: &ResourceBook) -> Result<(Self, ResourceBook), ResourceError> {
        let (mmio, virtio_mmio) = book
            .virtio_mmio
            .allocate(VIRTIO_MMIO_LEN, VIRTIO_MMIO_LEN)?;
        let (gsi, legacy_gsi) = book.legacy_gsi.allocate()?;
        let book = ResourceBook {
            virtio_mmio,
            legacy_gsi,
        };
        Ok((Self { mmio, gsi }, book))
    }
}

pub struct MachinePlan {
    pub spec: MachineSpec,
    pub ram: Vec<MemoryRegion>,
    /// One grant per disk, in `spec.disks` order.
    pub devices: Vec<VirtioMmioGrant>,
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
        let mut book = resource_book();
        let mut devices = Vec::with_capacity(spec.disks.len());
        for disk in &spec.disks {
            let (grant, next) = VirtioMmioGrant::allocate(&book).with_context(|| {
                format!(
                    "allocate virtio-mmio resources for disk {}",
                    disk.id.as_deref().unwrap_or_default()
                )
            })?;
            book = next;
            cmdline.add_virtio_mmio_device(
                grant.mmio.len(),
                GuestAddress(grant.mmio.start()),
                grant.gsi.0,
                None,
            )?;
            if disk.root {
                cmdline.insert("root", "/dev/vda")?;
                cmdline.insert_str(if disk.read_only { "ro" } else { "rw" })?;
            }
            devices.push(grant);
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

    fn data_disks(count: usize) -> Vec<BlockDevConfig> {
        (0..count)
            .map(|index| BlockDevConfig {
                id: Some(format!("disk{index}")),
                path: format!("/disk{index}").into(),
                read_only: false,
                root: false,
            })
            .collect()
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
        assert_eq!(
            plan.devices,
            [
                VirtioMmioGrant {
                    mmio: GpaRange::new(0xc000_0000, 0xc000_1000),
                    gsi: Gsi(5),
                },
                VirtioMmioGrant {
                    mmio: GpaRange::new(0xc000_1000, 0xc000_2000),
                    gsi: Gsi(6),
                },
            ]
        );
        assert_eq!(plan.ram.len(), 2);
        let cmdline = plan.cmdline.as_cstring().unwrap();
        let cmdline = cmdline.to_str().unwrap();
        assert!(cmdline.contains("virtio_mmio.device=4K@0xc0000000:5"));
        assert!(cmdline.contains("virtio_mmio.device=4K@0xc0001000:6"));
        assert!(cmdline.contains("root=/dev/vda ro"));
    }

    #[test]
    fn disks_beyond_the_legacy_gsis_exhaust_the_pool() {
        let mut spec = fixture();
        spec.disks = data_disks(19);
        let plan = MachinePlan::new(spec.clone(), &host()).unwrap();
        assert_eq!(
            plan.devices.last(),
            Some(&VirtioMmioGrant {
                mmio: GpaRange::new(0xc001_2000, 0xc001_3000),
                gsi: Gsi(23),
            })
        );

        spec.disks = data_disks(20);
        let Err(error) = MachinePlan::new(spec, &host()) else {
            panic!("planned more disks than legacy GSIs");
        };
        assert_eq!(
            error.downcast_ref::<ResourceError>(),
            Some(&ResourceError::Exhausted { pool: "legacy GSI" })
        );
        assert!(
            format!("{error:#}").ends_with("disk19: legacy GSI pool exhausted"),
            "{error:#}"
        );
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
