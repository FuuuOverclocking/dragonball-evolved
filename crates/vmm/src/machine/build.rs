use std::fs::File;
use std::io::Read;
use std::range::Range;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use kvm_bindings::{KVM_MP_STATE_UNINITIALIZED, Msrs, kvm_mp_state};
use kvm_ioctls::VmFd;
use linux_loader::loader::elf::Elf;
use linux_loader::loader::{KernelLoader, PvhBootCapability, load_cmdline};
use tokio::sync::mpsc;
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend};

use super::MachineEvent;
use super::block::{self, AttachedBlock};
use super::bus::{Bus, BusDevice};
use super::memory::GuestRam;
use super::plan::{DevicePlan, MachinePlan};
use super::platform::{Platform, PlatformSnapshot};
use super::snapshot::DeviceSnapshot;
use super::vcpu::{VcpuHandle, Vcpus};
use super::vm::{KvmVcpu, KvmVm};
use crate::KvmContext;
use crate::arch::x86_64::{cpuid, lapic, layout, mptable, msr, pvh, regs};
use crate::devices::DeviceManager;
use crate::devices::virtio::SharedGuestMemory;

pub struct Machine {
    pub vcpus: Option<Vcpus>,
    /// The device tasks: their stop signal, their exits and the I/O thread.
    pub tasks: DeviceManager,
    pub events: mpsc::UnboundedReceiver<MachineEvent>,
    pub(super) plan: MachinePlan,
    pub(super) platform: Platform,
    /// In plan order.
    pub(super) devices: Vec<AttachedDevice>,
    mmio: Arc<Bus>,
    pub(super) vm: Arc<KvmVm>,
}

/// A device attached to a machine, by kind.
pub(super) enum AttachedDevice {
    Block(AttachedBlock),
}

impl AttachedDevice {
    fn attach(
        plan: &DevicePlan,
        vm: &VmFd,
        memory: &SharedGuestMemory,
        tasks: &mut DeviceManager,
        mmio_bus: &mut Vec<(Range<u64>, Arc<dyn BusDevice>)>,
        restore: Option<DeviceSnapshot>,
    ) -> Result<Self> {
        match plan {
            DevicePlan::Block(plan) => {
                let restore = restore.map(|captured| match captured {
                    DeviceSnapshot::Block(captured) => captured,
                });
                block::attach(plan.clone(), vm, memory, tasks, mmio_bus, restore).map(Self::Block)
            }
        }
    }

    async fn quiesce(&mut self) {
        match self {
            Self::Block(block) => block.quiesce().await,
        }
    }

    async fn resume(&mut self) {
        match self {
            Self::Block(block) => block.resume().await,
        }
    }

    pub(super) fn capture(&self) -> Result<DeviceSnapshot> {
        match self {
            Self::Block(block) => block.capture().map(DeviceSnapshot::Block),
        }
    }
}

/// What [`assemble`] restores the platform and the devices from.
pub(super) struct Restore {
    pub platform: PlatformSnapshot,
    /// In plan order, one per planned device.
    pub devices: Vec<DeviceSnapshot>,
}

impl Machine {
    /// The layout this machine was built or restored with.
    pub fn plan(&self) -> &MachinePlan {
        &self.plan
    }

    /// The MMIO bus as the vCPUs see it.
    pub fn mmio_bus(&self) -> &Bus {
        &self.mmio
    }

    /// A view of the guest RAM.
    pub fn memory(&self) -> &SharedGuestMemory {
        self.vm.memory()
    }

    pub async fn pause(&mut self) -> Result<()> {
        self.vcpus.as_ref().unwrap().pause().await?;
        // Cooperatively quiesced devices record what they were doing as they park.
        for device in &mut self.devices {
            device.quiesce().await;
        }
        // Gate-based tasks: interrupt resamplers and the console input.
        self.tasks.quiesce().await;
        Ok(())
    }

    pub async fn resume(&mut self) -> Result<()> {
        self.tasks.resume().await;
        for device in &mut self.devices {
            device.resume().await;
        }
        self.vcpus.as_ref().unwrap().resume().await
    }

    /// Stops the vCPUs and the devices, then closes the VM and hands its guest
    /// RAM back.
    pub async fn teardown(mut self) -> Result<GuestRam> {
        let stopped = match self.vcpus.take() {
            Some(vcpus) => vcpus.stop().await,
            None => Ok(()),
        };
        self.tasks.stop().await;
        stopped?;
        KvmVm::into_ram(self.vm).map_err(|_| anyhow!("a vCPU still holds the VM"))
    }
}

pub fn build(plan: MachinePlan, context: &KvmContext) -> Result<Machine> {
    let vm = KvmVm::new(context, GuestRam::map(&plan.memory)?)?;
    let memory = Arc::clone(vm.memory());

    let kernel = plan.spec.kernel.as_ref().unwrap();
    let mut image = File::open(&kernel.path)
        .with_context(|| format!("open kernel {}", kernel.path.display()))?;
    let loaded = Elf::load(
        memory.as_ref(),
        None,
        &mut image,
        Some(GuestAddress(layout::HIMEM_START)),
    )
    .context("load ELF kernel")?;
    let entry = match loaded.pvh_boot_cap {
        PvhBootCapability::PvhEntryPresent(entry) => entry,
        _ => bail!("kernel has no PVH entry point"),
    };
    if entry.0 < layout::HIMEM_START
        || entry.0 >= layout::MMIO32_END
        || !memory.address_in_range(entry)
    {
        bail!("PVH entry is outside low guest RAM");
    }
    let initrd = if let Some(path) = &kernel.initrd {
        let mut file =
            File::open(path).with_context(|| format!("open initrd {}", path.display()))?;
        let size = file.metadata()?.len();
        if size == 0 {
            bail!("initrd is empty");
        }
        let low_end = plan.memory.ram[0].end();
        let address = layout::align_down(
            low_end
                .checked_sub(size)
                .context("initrd exceeds low RAM")?,
            layout::PAGE_SIZE,
        );
        if address < loaded.kernel_end || address < layout::HIMEM_START {
            bail!("initrd overlaps the loaded kernel");
        }
        let mut buffer = [0u8; 64 * 1024];
        let mut offset = 0;
        while offset < size {
            let count = (size - offset).min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..count])
                .context("read initrd")?;
            memory.write_slice(&buffer[..count], GuestAddress(address + offset))?;
            offset += count as u64;
        }
        Some(pvh::Initrd { address, size })
    } else {
        None
    };
    load_cmdline(
        memory.as_ref(),
        GuestAddress(layout::CMDLINE_START),
        &plan.cmdline,
    )?;
    for blob in pvh::boot_data(&plan.memory.ram, layout::CMDLINE_START, initrd)?
        .into_iter()
        .chain([mptable::build(plan.spec.cpus)?])
    {
        memory.write_slice(&blob.bytes, GuestAddress(blob.address))?;
    }

    let mut vcpus = Vec::with_capacity(plan.spec.cpus as usize);
    for index in 0..plan.spec.cpus {
        let vcpu = vm.create_vcpu(index)?;
        let caps = context.capabilities();
        vcpu.set_cpuid2(&cpuid::normalize(
            &caps.supported_cpuid,
            &caps.host_cpuid,
            plan.spec.cpus,
            index,
        )?)?;
        let msrs = msr::boot_msrs();
        if vcpu.set_msrs(&Msrs::from_entries(&msrs)?)? != msrs.len() {
            bail!("KVM did not set all boot MSRs");
        }
        if index == 0 {
            vcpu.set_regs(&regs::boot_regs(entry.0))?;
            vcpu.set_fpu(&regs::boot_fpu())?;
            let (sregs, blobs) = regs::boot_sregs(vcpu.get_sregs()?);
            for blob in blobs {
                memory.write_slice(&blob.bytes, GuestAddress(blob.address))?;
            }
            vcpu.set_sregs(&sregs)?;
        } else {
            vcpu.set_mp_state(kvm_mp_state {
                mp_state: KVM_MP_STATE_UNINITIALIZED,
            })?;
        }
        vcpu.set_lapic(&lapic::configure_lint(vcpu.get_lapic()?))?;
        vcpus.push(vcpu);
    }
    assemble(plan, vm, vcpus, None)
}

/// Attaches the platform and the planned devices, then starts the vCPU threads,
/// which park until resumed. Building and restoring share this path.
///
/// The vCPUs must already be configured or restored: a restored device can
/// interrupt the guest as soon as it is attached, and restoring the LAPIC after
/// that would drop the interrupt.
pub(super) fn assemble(
    plan: MachinePlan,
    vm: Arc<KvmVm>,
    vcpus: Vec<KvmVcpu>,
    restore: Option<Restore>,
) -> Result<Machine> {
    let (events_tx, events) = mpsc::unbounded_channel();
    let mut tasks = DeviceManager::new();
    let mut pio_bus = Vec::new();
    let mut mmio_bus = Vec::new();
    let (platform_state, device_states) = match restore {
        Some(restore) => (Some(restore.platform), restore.devices),
        None => (None, Vec::new()),
    };
    let platform = Platform::attach(
        plan.spec.console,
        vm.fd(),
        &events_tx,
        &mut tasks,
        &mut pio_bus,
        platform_state,
    )?;
    // Empty when building, so every device attaches fresh.
    let mut device_states = device_states.into_iter();
    let mut devices = Vec::with_capacity(plan.devices.len());
    for device in &plan.devices {
        devices.push(AttachedDevice::attach(
            device,
            vm.fd(),
            vm.memory(),
            &mut tasks,
            &mut mmio_bus,
            device_states.next(),
        )?);
    }
    let pio = Arc::new(Bus::new(pio_bus)?);
    let mmio = Arc::new(Bus::new(mmio_bus)?);
    let mut handles = Vec::with_capacity(vcpus.len());
    for (index, vcpu) in vcpus.into_iter().enumerate() {
        handles.push(VcpuHandle::spawn(
            index as u8,
            vcpu,
            Arc::clone(&pio),
            Arc::clone(&mmio),
            events_tx.clone(),
        )?);
    }
    Ok(Machine {
        vcpus: Some(Vcpus::new(handles)),
        tasks,
        events,
        plan,
        platform,
        devices,
        mmio,
        vm,
    })
}
