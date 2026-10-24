//! Pure-data snapshot types for capturing VM state.
//!
//! These types hold extracted state that can later be serialised and restored.
//! They carry no behaviour and no references to live kernel objects.

use std::range::Range;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use kvm_bindings::{
    CpuId, KVM_PIT_SPEAKER_DUMMY, Msrs, kvm_clock_data, kvm_debugregs, kvm_irqchip,
    kvm_lapic_state, kvm_mp_state, kvm_msr_entry, kvm_pit_config, kvm_pit_state2, kvm_regs,
    kvm_sregs, kvm_vcpu_events, kvm_xcrs, kvm_xsave,
};
use kvm_ioctls::{IoEventAddress, VmFd};
use logger::warn;
use tokio::sync::mpsc;
use virtio_queue::{Queue, QueueState, QueueT};
use vm_superio::SerialState;

use super::build::Machine;
use super::bus::{Bus, BusDevice};
use super::memory;
use super::memory::SharedGuestMemory;
use super::plan::MachinePlan;
use super::vcpu::{VcpuHandle, Vcpus};
use crate::KvmContext;
use crate::arch::x86_64::layout;
use crate::devices::legacy::console;
use crate::devices::legacy::i8042::{I8042Device, I8042State};
use crate::devices::legacy::serial::SerialDevice;
use crate::devices::virtio::block::{BlockQuiesced, BlockResources};
use crate::devices::virtio::mmio::{MmioRegs, MmioTransport, NOTIFY_OFFSET};
use crate::devices::virtio::{Activation, session_channel};
use crate::devices::{DeviceManager, DeviceRole, Placement};
use crate::spec::MachineSpec;

pub struct MachineSnapshot {
    pub vm: VmKvmState,
    pub vcpus: Vec<VcpuSnapshot>,
    pub devices: DeviceSnapshots,
    pub memory: SharedGuestMemory,
}

pub struct VmKvmState {
    /// PIC master (chip_id = 0).
    pub pic_master: kvm_irqchip,
    /// PIC slave (chip_id = 1).
    pub pic_slave: kvm_irqchip,
    /// IOAPIC (chip_id = 2).
    pub ioapic: kvm_irqchip,
    pub pit: kvm_pit_state2,
    pub clock: kvm_clock_data,
}

pub struct VcpuSnapshot {
    pub index: u8,
    pub mp_state: kvm_mp_state,
    pub regs: kvm_regs,
    pub sregs: kvm_sregs,
    pub xsave: kvm_xsave,
    pub lapic: kvm_lapic_state,
    pub cpuid: CpuId,
    pub msrs: Vec<kvm_msr_entry>,
    pub xcrs: kvm_xcrs,
    pub debug_regs: kvm_debugregs,
    pub vcpu_events: kvm_vcpu_events,
}

pub struct DeviceSnapshots {
    pub serial: Option<SerialSnapshot>,
    pub i8042: I8042State,
    /// One per disk, in plan order.
    pub transports: Vec<MmioTransportSnapshot>,
    /// One per disk, in plan order: the queue of a device that had an active
    /// session when the machine paused, or `None`.
    pub block_queues: Vec<Option<QueueState>>,
}

pub struct SerialSnapshot {
    pub state: SerialState,
    pub interrupt_status: u32,
}

pub struct MmioTransportSnapshot {
    pub regs: MmioRegs,
    pub interrupt_status: u32,
}

// ---------------------------------------------------------------------------
// Capture helpers
// ---------------------------------------------------------------------------

/// Extract the KVM irqchip (PIC master, PIC slave, IOAPIC), PIT2, and clock.
pub fn capture_vm_kvm(vm: &VmFd) -> Result<VmKvmState> {
    let mut pic_master = kvm_irqchip {
        chip_id: 0,
        ..Default::default()
    };
    vm.get_irqchip(&mut pic_master).context("get PIC master")?;

    let mut pic_slave = kvm_irqchip {
        chip_id: 1,
        ..Default::default()
    };
    vm.get_irqchip(&mut pic_slave).context("get PIC slave")?;

    let mut ioapic = kvm_irqchip {
        chip_id: 2,
        ..Default::default()
    };
    vm.get_irqchip(&mut ioapic).context("get IOAPIC")?;

    let pit = vm.get_pit2().context("get PIT2")?;
    let clock = vm.get_clock().context("get clock")?;

    Ok(VmKvmState {
        pic_master,
        pic_slave,
        ioapic,
        pit,
        clock,
    })
}

/// Capture every vCPU's register state through the duplicate kick fds.
pub fn capture_vcpus(vcpus: &Vcpus) -> Result<Vec<VcpuSnapshot>> {
    vcpus.handles().iter().map(|h| h.capture_state()).collect()
}

/// Capture mutex-reachable device state.
///
/// Block device state comes from `machine.captured_blocks`, which is recorded
/// during [`Machine::pause`]. A machine with an exited block device is refused:
/// there is no state to capture for it, and restoring it would hide the failure.
pub fn capture_devices(machine: &Machine) -> Result<DeviceSnapshots> {
    let serial = machine.serial_handle.as_ref().map(|h| {
        let state = h.uart.lock().unwrap().state();
        SerialSnapshot {
            state,
            interrupt_status: 0,
        }
    });

    let i8042 = machine
        .i8042
        .as_ref()
        .map_or(I8042State::default(), |d| d.snapshot_state());

    let transports = machine
        .mmio_transports
        .iter()
        .map(|t| {
            let (regs, status) = t.snapshot();
            MmioTransportSnapshot {
                regs,
                interrupt_status: status,
            }
        })
        .collect();

    let block_queues = machine
        .captured_blocks
        .iter()
        .zip(&machine.block_quiesce)
        .map(|(captured, handle)| match *captured {
            BlockQuiesced::Inactive => Ok(None),
            BlockQuiesced::Active(queue) => Ok(Some(queue)),
            BlockQuiesced::Exited => Err(anyhow!(
                "block device {} has exited; refusing to snapshot",
                handle.id
            )),
        })
        .collect::<Result<_>>()?;

    Ok(DeviceSnapshots {
        serial,
        i8042,
        transports,
        block_queues,
    })
}

/// Capture the full machine snapshot (VM KVM state, vCPUs, devices, memory).
pub fn capture(machine: &Machine) -> Result<MachineSnapshot> {
    let devices = capture_devices(machine)?;
    let vcpus = capture_vcpus(machine.vcpus.as_ref().unwrap())?;
    let vm = capture_vm_kvm(&machine.vm)?;
    let memory = Arc::clone(&machine.memory);
    Ok(MachineSnapshot {
        vm,
        vcpus,
        devices,
        memory,
    })
}

// ---------------------------------------------------------------------------
// Rebuild
// ---------------------------------------------------------------------------

/// Reconstruct a [`Machine`] from a [`MachineSnapshot`] and the original
/// [`MachineSpec`].
///
/// The returned machine is in a paused-equivalent state: vCPUs are spawned
/// paused, devices are live, and block devices that had an active session when
/// captured are reactivated with their captured queue state.
pub fn rebuild(
    snapshot: MachineSnapshot,
    spec: &MachineSpec,
    context: &KvmContext,
) -> Result<Machine> {
    // Until snapshots record the plan, re-plan so each disk keeps build()'s address and GSI.
    let plan = MachinePlan::new(spec.clone(), context.capabilities())?;
    let disks = plan.spec.disks.len();
    if snapshot.devices.transports.len() != disks || snapshot.devices.block_queues.len() != disks {
        bail!(
            "snapshot has {} transport and {} block records for {disks} disks",
            snapshot.devices.transports.len(),
            snapshot.devices.block_queues.len(),
        );
    }

    // 1. Create KVM VM.
    let vm = context.kvm().create_vm().context("create KVM VM")?;
    vm.set_tss_address(layout::TSS_START as usize)
        .context("set TSS address")?;

    // 2. Restore interrupt controllers.
    vm.create_irq_chip().context("create irqchip")?;
    vm.set_irqchip(&snapshot.vm.pic_master)
        .context("restore PIC master")?;
    vm.set_irqchip(&snapshot.vm.pic_slave)
        .context("restore PIC slave")?;
    vm.set_irqchip(&snapshot.vm.ioapic)
        .context("restore IOAPIC")?;
    vm.create_pit2(kvm_pit_config {
        flags: KVM_PIT_SPEAKER_DUMMY,
        ..Default::default()
    })
    .context("create PIT2")?;
    vm.set_pit2(&snapshot.vm.pit).context("restore PIT2")?;

    // 3. Register memory (the snapshot's Arc keeps the mmap alive).
    // SAFETY: The snapshot's Arc<GuestMemoryMmap> guarantees the mmap region
    // outlives the VM. We drop the VM before releasing the last Arc.
    unsafe {
        memory::register(
            &vm,
            snapshot.memory.as_ref(),
            context.capabilities().max_memslots,
        )
    }?;

    let (events_tx, events) = mpsc::unbounded_channel();

    // 4. Rebuild devices and buses.
    let i8042 = Arc::new(I8042Device::from_state(
        snapshot.devices.i8042,
        events_tx.clone(),
    ));
    let mut pio_devices: Vec<(Range<u64>, Arc<dyn BusDevice>)> = vec![(
        layout::I8042_PORTS,
        Arc::clone(&i8042) as Arc<dyn BusDevice>,
    )];
    let mut mmio_devices: Vec<(Range<u64>, Arc<dyn BusDevice>)> = Vec::new();
    let mut mmio_transports: Vec<Arc<MmioTransport>> = Vec::new();
    let mut blocks = Vec::new();
    let mut block_quiesce = Vec::new();
    let mut captured_blocks = Vec::new();
    let mut devices = DeviceManager::new();

    let records = snapshot
        .devices
        .transports
        .iter()
        .zip(&snapshot.devices.block_queues);
    for ((disk, grant), (transport_snapshot, queue_state)) in
        plan.spec.disks.iter().zip(&plan.devices).zip(records)
    {
        let (resources, handle, quiesce_handle) =
            BlockResources::open(disk.id.as_ref().unwrap(), &disk.path, disk.read_only, None)?;
        vm.register_ioevent(
            &handle.queue_notifier,
            &IoEventAddress::Mmio(grant.mmio.start() + NOTIFY_OFFSET),
            0u32,
        )?;
        vm.register_irqfd_with_resample(
            handle.interrupt.irq_fd(),
            handle.interrupt.resample_fd(),
            grant.gsi.0,
        )?;
        let stop = devices.stop_signal();
        devices.spawn(
            format!("irq-{}", grant.gsi.0),
            DeviceRole::GuestAffecting,
            Arc::clone(&handle.interrupt).resample(stop),
        );

        // Clone the interrupt Arc before it is moved into the transport, so
        // we can pass it into the reactivation path if needed.
        let interrupt_for_activation = Arc::clone(&handle.interrupt);

        let transport = Arc::new(MmioTransport::with_state(
            handle.desc,
            Arc::clone(&snapshot.memory),
            handle.activator.clone(),
            handle.interrupt,
            vec![handle.queue_notifier],
            transport_snapshot.regs.clone(),
            transport_snapshot.interrupt_status,
        ));
        mmio_devices.push((
            grant.mmio.into(),
            Arc::clone(&transport) as Arc<dyn BusDevice>,
        ));
        mmio_transports.push(Arc::clone(&transport));
        block_quiesce.push(quiesce_handle);
        captured_blocks.push(queue_state.map_or(BlockQuiesced::Inactive, BlockQuiesced::Active));

        // The block record, not the transport status, decides reactivation.
        blocks.push((
            disk.id.as_ref().unwrap().clone(),
            resources,
            handle.activator,
            *queue_state,
            transport_snapshot.regs.driver_features,
            interrupt_for_activation,
            transport,
        ));
    }

    // Serial restore.
    let mut serial_handle = None;
    if let Some(serial_snap) = &snapshot.devices.serial {
        let (output, drain) = console::output(console::DEFAULT_OUTPUT_CAPACITY);
        let (serial, handle) =
            SerialDevice::from_state(&serial_snap.state, None, output).context("restore serial")?;
        let handle = Arc::new(handle);
        vm.register_irqfd(&handle.irq_fd, layout::COM1_GSI.0)?;
        pio_devices.push((
            layout::COM1_PORTS,
            Arc::clone(&handle) as Arc<dyn BusDevice>,
        ));
        serial_handle = Some(handle);
        let stop = devices.stop_signal();
        devices.spawn(
            "console-input",
            DeviceRole::GuestAffecting,
            serial.run(stop),
        );
        let stop = devices.stop_signal();
        devices.spawn(
            "console-output",
            DeviceRole::HostOnly,
            drain.run_stdout(stop),
        );
    }

    let pio = Arc::new(Bus::new(pio_devices)?);
    let mmio = Arc::new(Bus::new(mmio_devices)?);

    // 5. Create vCPUs and restore register state.
    let mut fds = Vec::with_capacity(snapshot.vcpus.len());
    for vcpu_snap in &snapshot.vcpus {
        let vcpu = vm
            .create_vcpu(u64::from(vcpu_snap.index))
            .with_context(|| format!("create vCPU {}", vcpu_snap.index))?;
        // Restore ordering follows Firecracker: cpuid first (BSP detection
        // depends on it), then mp_state, general regs, sregs, fpu, xcrs,
        // debug regs, lapic (after sregs), MSRs, vcpu_events last.
        vcpu.set_cpuid2(&vcpu_snap.cpuid).context("restore cpuid")?;
        vcpu.set_mp_state(vcpu_snap.mp_state)
            .context("restore mp_state")?;
        vcpu.set_regs(&vcpu_snap.regs).context("restore regs")?;
        vcpu.set_sregs(&vcpu_snap.sregs).context("restore sregs")?;
        // SAFETY: We restore the same kvm_xsave that get_xsave() produced on the
        // same host. No dynamic XSTATE features are enabled, so the traditional
        // 4096-byte kvm_xsave is sufficient.
        unsafe { vcpu.set_xsave(&vcpu_snap.xsave) }.context("restore xsave")?;
        vcpu.set_xcrs(&vcpu_snap.xcrs).context("restore xcrs")?;
        vcpu.set_debug_regs(&vcpu_snap.debug_regs)
            .context("restore debug_regs")?;
        vcpu.set_lapic(&vcpu_snap.lapic).context("restore lapic")?;
        let msrs = Msrs::from_entries(&vcpu_snap.msrs).context("build MSR set")?;
        let set_count = vcpu.set_msrs(&msrs).context("restore msrs")?;
        if set_count != vcpu_snap.msrs.len() {
            bail!(
                "KVM set {set_count}/{} MSRs for vCPU {}",
                vcpu_snap.msrs.len(),
                vcpu_snap.index
            );
        }
        vcpu.set_vcpu_events(&vcpu_snap.vcpu_events)
            .context("restore vcpu_events")?;
        // Notify KVM that the guest's pvclock may be out of sync after restore,
        // preventing spurious soft-lockup detection inside the guest.
        if let Err(e) = vcpu.kvmclock_ctrl() {
            warn!("kvmclock_ctrl failed for vCPU {}: {e:#}", vcpu_snap.index);
        }
        fds.push(vcpu);
    }

    // 6. Restore VM clock (after vCPU setup, before running).
    vm.set_clock(&snapshot.vm.clock)
        .context("restore VM clock")?;

    // 7. Spawn block device tasks and handle reactivation.
    for (name, resources, activator, queue_state, driver_features, interrupt, transport) in blocks {
        let mem = Arc::clone(&snapshot.memory);
        // Create the session channel outside the async block so we can inject
        // the control half into the MmioTransport synchronously.
        let reactivate = if let Some(qs) = queue_state {
            let (control, session) = session_channel();
            transport.inject_session(control);
            Some((qs, activator, mem, driver_features, interrupt, session))
        } else {
            None
        };
        devices.spawn_on(
            name,
            Placement::Io,
            DeviceRole::HostOnly,
            move |stop| async move {
                let device = resources.into_device()?;
                // If the device was active before the snapshot, send an
                // activation with the captured queue state so it resumes I/O.
                if let Some((qs, activator, mem, driver_features, interrupt, session)) = reactivate
                {
                    let queue = Queue::try_from(qs).context("restore block queue from state")?;
                    if !queue.is_valid(mem.as_ref()) {
                        bail!("restored block queue failed validation");
                    }
                    activator.activate(Activation {
                        mem,
                        queues: vec![queue],
                        interrupt,
                        driver_features,
                        session,
                    })?;
                    device.run(stop).await
                } else {
                    device.run(stop).await
                }
            },
        )?;
    }

    // 9. Spawn vCPU threads (paused).
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
        serial_handle,
        i8042: Some(i8042),
        mmio_transports,
        pio,
        mmio,
        vm,
        memory: snapshot.memory,
        raw_mode: None,
        block_quiesce,
        captured_blocks,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use kvm_bindings::{KVM_PIT_SPEAKER_DUMMY, kvm_pit_config};

    use super::*;
    use crate::KvmContext;
    use crate::arch::x86_64::layout::{TSS_START, ram_regions};
    use crate::machine::memory;

    #[test]
    fn capture_vm_kvm_returns_irqchip_and_pit_on_kvm() {
        if !Path::new("/dev/kvm").exists() {
            return;
        }
        let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
        let vm = context.kvm().create_vm().unwrap();
        vm.set_tss_address(TSS_START as usize).unwrap();
        vm.create_irq_chip().unwrap();
        vm.create_pit2(kvm_pit_config {
            flags: KVM_PIT_SPEAKER_DUMMY,
            ..Default::default()
        })
        .unwrap();

        let regions = ram_regions(16 << 20).unwrap();
        let mem = memory::create(&regions).unwrap();
        // SAFETY: test drops VM before memory.
        unsafe { memory::register(&vm, mem.as_ref(), context.capabilities().max_memslots) }
            .unwrap();

        let state = capture_vm_kvm(&vm).unwrap();
        assert_eq!(state.pic_master.chip_id, 0);
        assert_eq!(state.pic_slave.chip_id, 1);
        assert_eq!(state.ioapic.chip_id, 2);

        drop(vm);
    }
}
