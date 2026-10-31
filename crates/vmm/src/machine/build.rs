//! Building a machine from its plan: read the files, compute the boot data,
//! write it, then assemble the vCPUs and the platform, all parked.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use linux_loader::loader::elf::{Elf, PvhBootCapability};
use linux_loader::loader::{self, KernelLoader};
use tokio::sync::mpsc;
use vm_memory::{Bytes, GuestAddress, GuestMemoryError};

use super::MachineEvents;
use super::boot::{self, BootDataError};
use super::plan::MachinePlan;
use super::platform::{ConsoleBackend, Platform, PlatformError};
use super::state::{Parts, PausedMachine};
use super::vcpu::{self, Buses, ConfigureVcpuError, SpawnVcpuError, Vcpus};
use crate::arch::x86_64::layout::HIMEM_START;
use crate::bus::{BusError, MmioBus, PioBus};
use crate::devices::runtime::DeviceRuntime;
use crate::kvm_context::KvmContext;
use crate::memory::policy::HostPolicy;
use crate::memory::store::{MaterializeError, MemoryStore};
use crate::space::{Gpa, Pio};
use crate::vm::{CreateVmError, KvmVcpuError, create_vm};

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
    /// attach the platform devices
    Platform(#[from] PlatformError),
    /// assemble the PIO bus
    PioBus(#[from] BusError<Pio>),
    /// start the vCPUs
    Vcpus(#[from] SpawnVcpuError),
}

/// Builds the machine `plan` describes, with guest memory held as `policy`
/// says. It comes out paused: device tasks and vCPU threads have started
/// parked, and nothing has run guest code yet.
///
/// Must run on the thread whose local runtime the device tasks live on.
/// Reading the kernel and the initrd blocks that thread. On failure, whatever
/// was built is released: memory, the VM, and the terminal mode.
pub fn build(
    plan: MachinePlan,
    policy: HostPolicy,
    context: &KvmContext,
) -> Result<(PausedMachine, MachineEvents), BuildError> {
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
) -> Result<(PausedMachine, MachineEvents), BuildError> {
    if tokio::runtime::Handle::try_current().is_err() {
        return Err(BuildError::NoRuntime);
    }
    // Read: open the files first, so that a missing one costs nothing.
    let mut kernel = File::open(plan.kernel()).map_err(|source| BuildError::OpenKernel {
        path: plan.kernel().to_owned(),
        source,
    })?;
    let initrd = plan.initrd().map(Initrd::open).transpose()?;
    let low_ram = boot::low_ram(plan.memory())?;

    let host = context.capabilities();
    let memory = MemoryStore::materialize(plan.memory().clone(), policy, host)?;
    let (vm, bindings, memslots) = create_vm(context, memory)?;
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
        return Err(BuildError::NoPvhEntry(plan.kernel().to_owned()));
    };
    let entry = Gpa(entry.0);
    boot::check_entry(low_ram, entry)?;
    let initrd = match initrd {
        Some(mut initrd) => {
            let placed = boot::place_initrd(low_ram, Gpa(loaded.kernel_end), initrd.size)?;
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
    for blob in boot::boot_data(&plan, initrd)? {
        view.write_slice(&blob.bytes, GuestAddress(blob.address.0))
            .map_err(BuildError::WriteBootData)?;
    }
    let count = plan.vcpus();
    let mut vcpus = Vec::with_capacity(usize::from(count));
    for index in 0..count {
        let vcpu = vm.create_vcpu(index)?;
        vcpu::configure(&vcpu, host, count, index, entry, &view)?;
        vcpus.push(vcpu);
    }

    // Assemble, everything parked.
    let (events, machine_events) = mpsc::unbounded_channel();
    let (mut runtime, device_exits) = DeviceRuntime::new();
    let mut pio = Vec::new();
    let platform = Platform::attach(
        plan.has_com1(),
        console,
        &vm,
        &events,
        &mut runtime,
        &mut pio,
    )?;
    let buses = Arc::new(Buses {
        pio: PioBus::new(pio)?,
        mmio: MmioBus::default(),
    });
    let vcpus = Vcpus::spawn(vcpus, buses, events)?;
    let parts = Box::new(Parts {
        vcpus,
        runtime,
        platform,
        bindings,
        memslots,
        vm,
        plan,
    });
    let events = MachineEvents {
        machine: machine_events,
        devices: device_exits,
    };
    Ok((PausedMachine::built(parts), events))
}
