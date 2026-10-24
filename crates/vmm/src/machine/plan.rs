use anyhow::{Context, Result};
use linux_loader::cmdline::Cmdline;

use super::block::{self, BlockPlan};
use super::memory::MemoryPlan;
use crate::arch::x86_64::layout::{CMDLINE_MAX_SIZE, VIRTIO_MMIO_LEN, ram_regions, resource_book};
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
    pub(super) fn allocate(book: &ResourceBook) -> Result<(Self, ResourceBook), ResourceError> {
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

    fn reserve(self, book: &ResourceBook) -> Result<ResourceBook, ResourceError> {
        Ok(ResourceBook {
            virtio_mmio: book.virtio_mmio.reserve(self.mmio)?,
            legacy_gsi: book.legacy_gsi.reserve(self.gsi)?,
        })
    }
}

/// Every layout decision for a machine, made once from its spec and then kept
/// as data: a snapshot records it and a restore replays it.
#[derive(Debug, Clone, PartialEq)]
pub struct MachinePlan {
    pub spec: MachineSpec,
    pub memory: MemoryPlan,
    /// In the order the guest probes them.
    pub devices: Vec<DevicePlan>,
    pub cmdline: Cmdline,
}

/// Where one device sits in the machine, by kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicePlan {
    Block(BlockPlan),
}

impl DevicePlan {
    pub fn id(&self) -> &str {
        match self {
            Self::Block(block) => &block.id,
        }
    }
}

impl MachinePlan {
    pub fn new(spec: MachineSpec, host: &HostCapabilities) -> Result<Self> {
        spec.validate(host)?;
        let memory = MemoryPlan {
            backing: spec.memory_backing,
            regions: ram_regions(u64::from(spec.memory_mib) << 20)?,
        };
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
        let (blocks, _) = block::plan(spec.blocks(), resource_book(), &mut cmdline)?;
        let devices = blocks.into_iter().map(DevicePlan::Block).collect();
        Ok(Self {
            spec,
            memory,
            devices,
            cmdline,
        })
    }
}

/// Reserves every recorded grant again on a fresh book, so a plan whose grants
/// overlap or leave their pools is rejected instead of attached.
pub fn replay(devices: &[DevicePlan]) -> Result<ResourceBook> {
    devices.iter().try_fold(resource_book(), |book, device| {
        match device {
            DevicePlan::Block(block) => block.grant.reserve(&book),
        }
        .with_context(|| format!("replay the grant of device {}", device.id()))
    })
}

#[cfg(test)]
mod tests {
    use api::boot::KernelConfig;
    use kvm_bindings::CpuId;

    use super::*;
    use crate::arch::x86_64::cpuid::{CpuVendor, HostCpuid};
    use crate::spec::{BlockSpec, DeviceSpec};

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

    fn disk(id: &str, read_only: bool, root: bool) -> DeviceSpec {
        DeviceSpec::Block(BlockSpec {
            id: id.into(),
            path: format!("/{id}").into(),
            read_only,
            root,
        })
    }

    fn data_disks(count: usize) -> Vec<DeviceSpec> {
        (0..count)
            .map(|index| disk(&format!("disk{index}"), false, false))
            .collect()
    }

    fn grants(devices: &[DevicePlan]) -> Vec<VirtioMmioGrant> {
        devices
            .iter()
            .map(|device| match device {
                DevicePlan::Block(block) => block.grant,
            })
            .collect()
    }

    #[test]
    fn root_disk_is_first_and_cmdline_matches_the_resource_layout() {
        let mut spec = fixture();
        spec.memory_mib = 4096;
        spec.devices = vec![disk("data", false, false), disk("root", true, true)];
        let plan = MachinePlan::new(spec, &host()).unwrap();
        assert_eq!(
            plan.devices.iter().map(DevicePlan::id).collect::<Vec<_>>(),
            ["root", "data"]
        );
        assert_eq!(
            grants(&plan.devices),
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
        assert_eq!(plan.memory.regions.len(), 2);
        let cmdline = plan.cmdline.as_cstring().unwrap();
        let cmdline = cmdline.to_str().unwrap();
        assert!(cmdline.contains("virtio_mmio.device=4K@0xc0000000:5"));
        assert!(cmdline.contains("virtio_mmio.device=4K@0xc0001000:6"));
        assert!(cmdline.contains("root=/dev/vda ro"));
    }

    #[test]
    fn disks_beyond_the_legacy_gsis_exhaust_the_pool() {
        let mut spec = fixture();
        spec.devices = data_disks(19);
        let plan = MachinePlan::new(spec.clone(), &host()).unwrap();
        assert_eq!(
            grants(&plan.devices).last(),
            Some(&VirtioMmioGrant {
                mmio: GpaRange::new(0xc001_2000, 0xc001_3000),
                gsi: Gsi(23),
            })
        );

        spec.devices = data_disks(20);
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
        let mut spec = fixture();
        spec.devices = vec![disk("same", false, false), disk("same", false, false)];
        assert!(MachinePlan::new(spec, &host()).is_err());
    }

    #[test]
    fn replay_accepts_a_fresh_plan_and_rejects_a_grant_held_twice() {
        let mut spec = fixture();
        spec.devices = data_disks(3);
        let plan = MachinePlan::new(spec, &host()).unwrap();
        // The replayed book holds exactly the planned grants.
        let book = replay(&plan.devices).unwrap();
        assert_eq!(
            VirtioMmioGrant::allocate(&book).unwrap().0,
            VirtioMmioGrant {
                mmio: GpaRange::new(0xc000_3000, 0xc000_4000),
                gsi: Gsi(8),
            }
        );

        let DevicePlan::Block(first) = &plan.devices[0];
        let holding = |id: &str, grant| {
            let mut devices = plan.devices.clone();
            devices.push(DevicePlan::Block(BlockPlan {
                id: id.into(),
                grant,
                ..first.clone()
            }));
            replay(&devices).unwrap_err()
        };
        let error = holding("copy", first.grant);
        assert_eq!(
            error.downcast_ref::<ResourceError>(),
            Some(&ResourceError::Overlap {
                pool: "virtio-mmio",
                range: first.grant.mmio,
                allocated: first.grant.mmio,
            })
        );
        assert!(format!("{error:#}").contains("device copy"), "{error:#}");
        // Each pool is replayed: a free window with a taken line is refused too.
        let error = holding(
            "same-line",
            VirtioMmioGrant {
                mmio: GpaRange::new(0xc000_5000, 0xc000_6000),
                gsi: first.grant.gsi,
            },
        );
        assert_eq!(
            error.downcast_ref::<ResourceError>(),
            Some(&ResourceError::IdInUse {
                pool: "legacy GSI",
                id: 5,
            })
        );
    }
}
