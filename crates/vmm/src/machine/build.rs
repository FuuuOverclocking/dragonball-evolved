//! Building a machine from its plan: read the files, compute the boot data,
//! write it, then assemble the vCPUs and the platform, all parked.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use linux_loader::loader::elf::{Elf, PvhBootCapability};
use linux_loader::loader::{self, KernelLoader};
use tokio::sync::mpsc;
use vm_memory::{Bytes, GuestAddress, GuestMemoryError};

use super::MachineEvents;
use super::block::{AttachBlockError, PreparedBlock};
use super::boot::{self, BootDataError};
use super::cleanup::{CleanupParts, PendingCleanup};
use super::mem::{AttachMemError, PreparedMem};
use super::plan::{DevicePlan, MachinePlan, PlanError};
use super::platform::{
    ConsoleBackend, Platform, PlatformError, PlatformQuiesced, PlatformSnapshot,
};
use super::state::{Parts, PausedMachine};
use super::vcpu::{self, Buses, ConfigureVcpuError, SpawnVcpuError, Vcpus};
use crate::arch::x86_64::layout::HIMEM_START;
use crate::bus::{BusError, MmioBus, PioBus};
use crate::devices::runtime::{DeviceRuntime, Response};
use crate::kvm_context::KvmContext;
use crate::memory::layout::WindowId;
use crate::memory::policy::HostPolicy;
use crate::memory::service::MemoryResources;
use crate::memory::store::{MaterializeError, MemoryStore};
use crate::space::{Gpa, Pio};
use crate::vm::{Binding, CreateVmError, KvmVcpu, KvmVcpuError, KvmVm, MemSlots, create_vm};

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum BuildError {
    /// building a machine needs a tokio runtime on this thread
    NoRuntime,
    /// open kernel {path}
    OpenKernel {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// open initrd {path}
    OpenInitrd {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// materialize guest memory
    Memory(#[from] MaterializeError),
    /// create the VM
    Vm(#[from] CreateVmError),
    /// load kernel {path}
    LoadKernel {
        path: PathBuf,
        #[source]
        source: loader::Error,
    },
    /// kernel {0} has no PVH entry point
    NoPvhEntry(PathBuf),
    /// place the boot data
    Boot(#[from] BootDataError),
    /// read initrd {path} into guest memory
    ReadInitrd {
        path: PathBuf,
        #[source]
        source: GuestMemoryError,
    },
    /// write the boot data
    WriteBootData(#[source] GuestMemoryError),
    /// create the vCPUs
    CreateVcpu(#[from] KvmVcpuError),
    /// configure the vCPUs
    ConfigureVcpu(#[from] ConfigureVcpuError),
    /// validate the recorded machine plan on this host
    Plan(#[from] PlanError),
    /// attach a block device
    Block(#[from] AttachBlockError),
    /// attach a memory device
    Mem(#[from] AttachMemError),
    /// no prepared device for plan ID {0}
    MissingPreparedDevice(String),
    /// prepared device {0} is not in the plan
    ExtraPreparedDevice(String),
    /// prepared device {0} has the wrong kind
    PreparedDeviceKind(String),
    /// attach the platform devices
    Platform(#[from] PlatformError),
    /// assemble the MMIO bus
    MmioBus(#[from] BusError<Gpa>),
    /// assemble the PIO bus
    PioBus(#[from] BusError<Pio>),
    /// start the vCPUs
    Vcpus(#[from] SpawnVcpuError),
}

/// Resources retained after a cold-build failure.
#[derive(Debug)]
pub enum BuildCleanup {
    /// No tasks or CPU threads were started; temporary resources have been released.
    Unstarted,
    /// Assembly retained ownership; the caller must wait before allowing a retry.
    Pending(PendingCleanup),
}

/// build the machine
#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub struct BuildFailure {
    #[source]
    pub error: BuildError,
    pub cleanup: BuildCleanup,
}

impl From<BuildError> for BuildFailure {
    fn from(error: BuildError) -> Self {
        Self {
            error,
            cleanup: BuildCleanup::Unstarted,
        }
    }
}

/// Builds the machine `plan` describes, with guest memory held as `policy`
/// says. It comes out paused: device tasks and vCPU threads have started
/// parked, and nothing has run guest code yet.
///
/// Must run on the thread whose local runtime the device tasks live on.
/// Reading the kernel and the initrd blocks that thread. Early failures release
/// unstarted resources; assembly failures return their owner in [`BuildFailure`].
/// Complete any pending cleanup before allowing another build.
pub fn build(
    plan: MachinePlan,
    policy: HostPolicy,
    context: &KvmContext,
) -> Result<(PausedMachine, MachineEvents), BuildFailure> {
    build_with(plan, policy, context, ConsoleBackend::Stdio)
}

/// An initrd opened for loading.
struct Initrd {
    path: PathBuf,
    file: File,
    size: u64,
}

impl Initrd {
    fn open(path: &Path) -> Result<Self, BuildError> {
        let error = |source| BuildError::OpenInitrd {
            path: path.to_owned(),
            source,
        };
        let file = File::open(path).map_err(error)?;
        let size = file.metadata().map_err(error)?.len();
        Ok(Self {
            path: path.to_owned(),
            file,
            size,
        })
    }
}

pub(super) fn build_with(
    plan: MachinePlan,
    policy: HostPolicy,
    context: &KvmContext,
    console: ConsoleBackend,
) -> Result<(PausedMachine, MachineEvents), BuildFailure> {
    if tokio::runtime::Handle::try_current().is_err() {
        return Err(BuildError::NoRuntime.into());
    }
    plan.replay(context.capabilities())
        .map_err(BuildError::Plan)?;
    // Read: open the files first, so that a missing one costs nothing.
    let mut kernel = File::open(plan.kernel()).map_err(|source| BuildError::OpenKernel {
        path: plan.kernel().to_owned(),
        source,
    })?;
    let initrd = plan.initrd().map(Initrd::open).transpose()?;
    let low_ram = boot::low_ram(plan.memory()).map_err(BuildError::Boot)?;

    let host = context.capabilities();
    let memory = MemoryStore::materialize(plan.memory().clone(), policy, host)
        .map_err(BuildError::Memory)?;
    let mut prepared = plan
        .devices()
        .iter()
        .filter_map(|(id, device)| device.block().map(|block| (id, block)))
        .map(|(id, block)| {
            PreparedBlock::cold(id, block, memory.view())
                .map(|block| (id.clone(), PreparedDevice::Block(block)))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()
        .map_err(BuildError::Block)?;
    let platform = PlatformSnapshot {
        i8042: Default::default(),
        serial: plan.has_com1().then(Default::default),
    };
    let (vm, bindings, mut memslots) = create_vm(context, memory).map_err(|(memory, error)| {
        // Cold boot allocated this RAM afresh; no captured guest state is lost.
        drop(memory);
        BuildError::Vm(error)
    })?;
    let view = Arc::clone(vm.memory().view());

    // The kernel is the one thing written straight from its file.
    let loaded = Elf::load(
        view.as_ref(),
        None,
        &mut kernel,
        Some(GuestAddress(HIMEM_START.0)),
    )
    .map_err(|source| BuildError::LoadKernel {
        path: plan.kernel().to_owned(),
        source,
    })?;
    let PvhBootCapability::PvhEntryPresent(entry) = loaded.pvh_boot_cap else {
        return Err(BuildError::NoPvhEntry(plan.kernel().to_owned()).into());
    };
    let entry = Gpa(entry.0);
    boot::check_entry(low_ram, entry).map_err(BuildError::Boot)?;
    let initrd = match initrd {
        Some(mut initrd) => {
            let placed = boot::place_initrd(low_ram, Gpa(loaded.kernel_end), initrd.size)
                .map_err(BuildError::Boot)?;
            view.read_exact_volatile_from(
                GuestAddress(placed.start().0),
                &mut initrd.file,
                placed.len() as usize,
            )
            .map_err(|source| BuildError::ReadInitrd {
                path: initrd.path,
                source,
            })?;
            Some(placed)
        }
        None => None,
    };

    // Compute, then write.
    for blob in boot::boot_data(&plan, initrd).map_err(BuildError::Boot)? {
        view.write_slice(&blob.bytes, GuestAddress(blob.address.0))
            .map_err(BuildError::WriteBootData)?;
    }
    let count = plan.vcpus();
    let mut vcpus = Vec::with_capacity(usize::from(count));
    for index in 0..count {
        let vcpu = vm.create_vcpu(index).map_err(BuildError::CreateVcpu)?;
        vcpu::configure(&vcpu, host, count, index, entry, &view)
            .map_err(BuildError::ConfigureVcpu)?;
        vcpus.push(vcpu);
    }

    for (id, device) in plan.devices() {
        if let Some(mem) = device.mem() {
            let permits = memslots
                .allocate(mem.geometry().chunk_count() as usize)
                .map_err(|source| AttachMemError::Slots {
                    id: id.clone(),
                    source,
                })
                .map_err(BuildError::Mem)?;
            let mem =
                PreparedMem::cold(id, mem, Arc::clone(&vm), permits).map_err(BuildError::Mem)?;
            prepared.insert(id.clone(), PreparedDevice::Mem(mem));
        }
    }
    drop(view);
    assemble_parked(
        plan, vm, bindings, memslots, vcpus, prepared, &platform, console,
    )
    .map_err(|(pending, error)| BuildFailure {
        error,
        cleanup: BuildCleanup::Pending(pending),
    })
}

pub(super) enum PreparedDevice {
    Block(PreparedBlock),
    Mem(PreparedMem),
}

pub(super) fn retain_prepared_memory(
    prepared: BTreeMap<String, PreparedDevice>,
    retained: &mut BTreeMap<WindowId, MemoryResources>,
) {
    for device in prepared.into_values() {
        if let PreparedDevice::Mem(mem) = device {
            let (window, resources) = mem.into_resources();
            assert!(retained.insert(window, resources).is_none());
        }
    }
}

fn validate_prepared(
    plan: &MachinePlan,
    prepared: &BTreeMap<String, PreparedDevice>,
) -> Result<(), BuildError> {
    for (id, device) in plan.devices() {
        match (device, prepared.get(id)) {
            (_, None) => return Err(BuildError::MissingPreparedDevice(id.clone())),
            (DevicePlan::Block(_), Some(PreparedDevice::Block(_)))
            | (DevicePlan::Mem(_), Some(PreparedDevice::Mem(_))) => {}
            _ => return Err(BuildError::PreparedDeviceKind(id.clone())),
        }
    }
    for id in prepared.keys() {
        if !plan.devices().contains_key(id) {
            return Err(BuildError::ExtraPreparedDevice(id.clone()));
        }
    }
    Ok(())
}

/// CPUs and VM state must already be installed before any device can inject an IRQ.
pub(super) fn assemble_parked(
    plan: MachinePlan,
    vm: Arc<KvmVm>,
    bindings: Vec<Binding>,
    memslots: MemSlots,
    vcpus: Vec<KvmVcpu>,
    mut prepared: BTreeMap<String, PreparedDevice>,
    platform: &PlatformSnapshot,
    console: ConsoleBackend,
) -> Result<(PausedMachine, MachineEvents), (PendingCleanup, BuildError)> {
    let quiesced = PlatformQuiesced {
        serial: platform
            .serial
            .as_ref()
            .map(|serial| Response::Replied(serial.input.clone())),
        output: (plan.has_com1() && matches!(&console, ConsoleBackend::Stdio))
            .then_some(Response::Replied(())),
    };
    let (events, machine_events) = mpsc::unbounded_channel();
    let (runtime, device_exits) = DeviceRuntime::new();
    let mut parts = Box::new(CleanupParts {
        vcpus: None,
        runtime,
        platform: None,
        blocks: BTreeMap::new(),
        mems: BTreeMap::new(),
        memory_services: BTreeMap::new(),
        memory_statuses: BTreeMap::new(),
        unstarted_memory: BTreeMap::new(),
        bindings,
        memslots,
        vm,
    });
    let assembled = (|| {
        validate_prepared(&plan, &prepared)?;
        let mut pio = Vec::new();
        parts.platform = Some(Platform::restore(
            plan.has_com1(),
            platform,
            console,
            &parts.vm,
            &events,
            &mut parts.runtime,
            &mut pio,
        )?);
        let mut mmio = Vec::new();
        for id in plan.probe_order() {
            let prepared = prepared.remove(id).expect("every plan ID was validated");
            match (&plan.devices()[id], prepared) {
                (DevicePlan::Block(plan), PreparedDevice::Block(prepared)) => {
                    let block =
                        prepared.attach(id, plan, &parts.vm, &mut parts.runtime, &mut mmio)?;
                    parts.blocks.insert(id.clone(), block);
                }
                (DevicePlan::Mem(plan), PreparedDevice::Mem(prepared)) => {
                    let mem = prepared.attach(id, plan, &mut parts, &mut mmio)?;
                    parts.mems.insert(id.clone(), mem);
                }
                _ => return Err(BuildError::PreparedDeviceKind(id.clone())),
            }
        }
        Ok::<_, BuildError>(Arc::new(Buses {
            pio: PioBus::new(pio)?,
            mmio: MmioBus::new(mmio)?,
        }))
    })();
    retain_prepared_memory(prepared, &mut parts.unstarted_memory);
    let buses = match assembled {
        Ok(buses) => buses,
        Err(error) => {
            drop(vcpus);
            return Err((PendingCleanup::tasks(parts), error));
        }
    };
    match Vcpus::spawn_retained(vcpus, buses, events) {
        Ok(vcpus) => parts.vcpus = Some(vcpus),
        Err((vcpus, error)) => {
            parts.vcpus = Some(vcpus);
            return Err((PendingCleanup::tasks(parts), BuildError::Vcpus(error)));
        }
    }
    let CleanupParts {
        vcpus,
        runtime,
        platform,
        blocks,
        mems,
        memory_services,
        memory_statuses: _,
        unstarted_memory,
        bindings,
        memslots,
        vm,
    } = *parts;
    assert!(unstarted_memory.is_empty());
    let parts = Box::new(Parts {
        vcpus: vcpus.expect("the CPU cohort was started"),
        runtime,
        platform: platform.expect("the platform was attached"),
        blocks,
        mems,
        memory_services,
        bindings,
        memslots,
        vm,
        plan,
    });
    let events = MachineEvents {
        machine: machine_events,
        devices: device_exits,
    };
    Ok((PausedMachine::built(parts, quiesced), events))
}

#[cfg(test)]
mod tests {
    use api::RawVmmOp;
    use api::block::{AddDisk, BlockDevConfig};
    use api::boot::{KernelConfig, SetKernel};
    use api::machine::{MemoryHotplugConfig, SetMemoryHotplug};
    use vm_memory::GuestMemoryMmap;

    use super::*;
    use crate::kvm_context::HostCapabilities;
    use crate::spec::{MachineSpec, VIRTIO_MEM_ID};
    use crate::test_guest::TempFile;

    #[test]
    fn early_build_failure_has_no_pending_owner_and_keeps_its_concrete_source() {
        use std::error::Error as _;

        let failure = BuildFailure::from(BuildError::NoRuntime);
        assert!(matches!(failure.cleanup, BuildCleanup::Unstarted));
        assert!(matches!(failure.error, BuildError::NoRuntime));
        assert!(matches!(
            failure.source().unwrap().downcast_ref::<BuildError>(),
            Some(BuildError::NoRuntime)
        ));
        assert_eq!(failure.to_string(), "build the machine");
    }

    #[test]
    fn prepared_devices_are_complete_and_matched_by_id_and_kind() {
        let disk = TempFile::new(&[0xab; 512]);
        let spec = MachineSpec::default()
            .apply(
                SetKernel(KernelConfig {
                    path: "/unused/kernel".into(),
                    cmdline: None,
                    initrd: None,
                })
                .into_vmm_command(),
            )
            .unwrap()
            .apply(
                AddDisk(BlockDevConfig {
                    id: Some("disk".into()),
                    path: disk.path().into(),
                    read_only: true,
                    root: false,
                })
                .into_vmm_command(),
            )
            .unwrap()
            .apply(
                SetMemoryHotplug(MemoryHotplugConfig {
                    size_mib: 128,
                    block_size_mib: None,
                    chunk_size_mib: None,
                })
                .into_vmm_command(),
            )
            .unwrap();
        let plan = MachinePlan::new(&spec, &HostCapabilities::fake(46, 32, 4096))
            .unwrap()
            .0;
        let mut prepared = BTreeMap::new();
        assert!(matches!(validate_prepared(&plan, &prepared),
            Err(BuildError::MissingPreparedDevice(id)) if id == "disk"
        ));
        let memory = Arc::new(GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap());
        let block = plan.devices()["disk"].block().unwrap();
        for id in ["disk", VIRTIO_MEM_ID] {
            prepared.insert(
                id.into(),
                PreparedDevice::Block(PreparedBlock::cold(id, block, &memory).unwrap()),
            );
        }
        assert!(matches!(validate_prepared(&plan, &prepared),
            Err(BuildError::PreparedDeviceKind(id)) if id == VIRTIO_MEM_ID
        ));
        drop(prepared);
    }
}
