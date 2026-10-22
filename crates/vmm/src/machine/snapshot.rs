//! Pure-data snapshot types for capturing VM state.
//!
//! These types hold extracted state that can later be serialised and restored.
//! They carry no behaviour and no references to live kernel objects.

use std::sync::Arc;

use anyhow::{Context, Result};
use kvm_bindings::{
    CpuId, kvm_clock_data, kvm_debugregs, kvm_fpu, kvm_irqchip, kvm_lapic_state, kvm_mp_state,
    kvm_msr_entry, kvm_pit_state2, kvm_regs, kvm_sregs, kvm_vcpu_events, kvm_xcrs,
};
use kvm_ioctls::VmFd;
use virtio_queue::QueueState;
use vm_superio::SerialState;

use super::build::Machine;
use super::memory::SharedGuestMemory;
use super::vcpu::Vcpus;
use crate::devices::legacy::i8042::I8042State;
use crate::devices::virtio::mmio::MmioRegs;

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
    pub fpu: kvm_fpu,
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
    pub transports: Vec<MmioTransportSnapshot>,
    pub block_queues: Vec<QueueState>,
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
/// Block queue states come from `machine.captured_block_queues`, which are
/// filled in during [`Machine::pause`] via cooperative quiesce.
pub fn capture_devices(machine: &Machine) -> DeviceSnapshots {
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

    DeviceSnapshots {
        serial,
        i8042,
        transports,
        block_queues: machine.captured_block_queues.clone(),
    }
}

/// Capture the full machine snapshot (VM KVM state, vCPUs, devices, memory).
pub fn capture(machine: &Machine) -> Result<MachineSnapshot> {
    let devices = capture_devices(machine);
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
