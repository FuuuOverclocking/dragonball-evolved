use std::fs::File;
use std::io::Read;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use api::machine::ConsoleConfig;
use kvm_bindings::{
    KVM_MP_STATE_UNINITIALIZED, KVM_PIT_SPEAKER_DUMMY, Msrs, kvm_mp_state, kvm_pit_config,
};
use kvm_ioctls::{IoEventAddress, VmFd};
use linux_loader::loader::elf::Elf;
use linux_loader::loader::{KernelLoader, PvhBootCapability, load_cmdline};
use tokio::sync::mpsc;
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend};

use super::bus::{Bus, BusDevice};
use super::plan::MachinePlan;
use super::vcpu::{VcpuHandle, Vcpus};
use super::{MachineEvent, memory};
use crate::KvmContext;
use crate::arch::x86_64::{cpuid, lapic, layout, mptable, msr, pvh, regs};
use crate::devices::legacy::console::{self, RawModeGuard};
use crate::devices::legacy::i8042::I8042Device;
use crate::devices::legacy::serial::SerialDevice;
use crate::devices::virtio::block::BlockResources;
use crate::devices::virtio::mmio::{MmioTransport, NOTIFY_OFFSET};
use crate::devices::{DeviceManager, Placement};

pub struct Machine {
    pub vcpus: Option<Vcpus>,
    pub devices: DeviceManager,
    pub events: mpsc::UnboundedReceiver<MachineEvent>,
    pio: Arc<Bus>,
    mmio: Arc<Bus>,
    vm: VmFd,
    memory: memory::SharedGuestMemory,
    raw_mode: Option<RawModeGuard>,
}

impl Machine {
    pub async fn teardown(mut self) -> Result<()> {
        let result = if let Some(vcpus) = self.vcpus.take() {
            vcpus.stop().await
        } else {
            Ok(())
        };
        self.devices.quiesce().await;
        drop(self.pio);
        drop(self.mmio);
        drop(self.vm);
        drop(self.memory);
        drop(self.raw_mode);
        result
    }
}

pub fn build(plan: MachinePlan, context: &KvmContext) -> Result<Machine> {
    let memory = memory::create(&plan.ram)?;
    let vm = context.kvm().create_vm().context("create KVM VM")?;
    vm.set_tss_address(layout::TSS_START as usize)
        .context("set TSS address")?;
    vm.create_irq_chip().context("create irqchip")?;
    vm.create_pit2(kvm_pit_config {
        flags: KVM_PIT_SPEAKER_DUMMY,
        ..Default::default()
    })
    .context("create PIT2")?;
    // SAFETY: Machine and startup rollback both stop vCPUs and drop VM before memory.
    unsafe { memory::register(&vm, memory.as_ref(), context.capabilities().max_memslots) }?;

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
        let low_end = plan.ram[0].end();
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
    for blob in pvh::boot_data(&plan.ram, layout::CMDLINE_START, initrd)?
        .into_iter()
        .chain([mptable::build(plan.spec.cpus)?])
    {
        memory.write_slice(&blob.bytes, GuestAddress(blob.address))?;
    }

    let (events_tx, events) = mpsc::unbounded_channel();
    let mut pio_devices: Vec<(std::ops::Range<u64>, Arc<dyn BusDevice>)> =
        vec![(0x60..0x65, Arc::new(I8042Device::new(events_tx.clone())))];
    let mut mmio_devices: Vec<(std::ops::Range<u64>, Arc<dyn BusDevice>)> = Vec::new();
    let mut blocks = Vec::new();
    let mut devices = DeviceManager::new();
    for (disk, location) in plan.spec.disks.iter().zip(&plan.devices) {
        let (resources, handle) =
            BlockResources::open(disk.id.as_ref().unwrap(), &disk.path, disk.read_only, None)?;
        vm.register_ioevent(
            &handle.queue_notifier,
            &IoEventAddress::Mmio(location.address + NOTIFY_OFFSET),
            0u32,
        )?;
        vm.register_irqfd_with_resample(
            handle.interrupt.irq_fd(),
            handle.interrupt.resample_fd(),
            location.gsi,
        )?;
        let stop = devices.stop_signal();
        devices.spawn(
            format!("irq-{}", location.gsi),
            Arc::clone(&handle.interrupt).resample(stop),
        );
        let transport = MmioTransport::new(
            handle.desc,
            Arc::clone(&memory),
            handle.activator,
            handle.interrupt,
            vec![handle.queue_notifier],
        );
        mmio_devices.push((
            location.address..location.address + layout::VIRTIO_MMIO_LEN,
            Arc::new(transport),
        ));
        blocks.push((disk.id.as_ref().unwrap().clone(), resources));
    }

    let mut raw_mode = None;
    if plan.spec.console == ConsoleConfig::Stdio {
        let input = console::open_input()?;
        let fd = input.map(|input| {
            raw_mode = input.raw_mode;
            input.fd
        });
        let (output, drain) = console::output(console::DEFAULT_OUTPUT_CAPACITY);
        let (serial, handle) = SerialDevice::with_input(fd, output)?;
        vm.register_irqfd(&handle.irq_fd, 4)?;
        pio_devices.push((0x3f8..0x400, Arc::new(handle)));
        let stop = devices.stop_signal();
        devices.spawn("console-input", serial.run(stop));
        let stop = devices.stop_signal();
        devices.spawn("console-output", drain.run_stdout(stop));
    }
    let pio = Arc::new(Bus::new(pio_devices)?);
    let mmio = Arc::new(Bus::new(mmio_devices)?);
    let mut fds = Vec::with_capacity(plan.spec.cpus as usize);
    for index in 0..plan.spec.cpus {
        let vcpu = vm.create_vcpu(u64::from(index)).context("create vCPU")?;
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
        fds.push(vcpu);
    }
    for (name, resources) in blocks {
        devices.spawn_on(name, Placement::Io, move |stop| async move {
            resources.into_device()?.run(stop).await
        })?;
    }
    let mut handles = Vec::with_capacity(fds.len());
    for (index, fd) in fds.into_iter().enumerate() {
        handles.push(VcpuHandle::spawn(
            index as u8,
            fd,
            &vm,
            Arc::clone(&pio),
            Arc::clone(&mmio),
            events_tx.clone(),
        )?);
    }
    Ok(Machine {
        vcpus: Some(Vcpus::new(handles)),
        devices,
        events,
        pio,
        mmio,
        vm,
        memory,
        raw_mode,
    })
}
