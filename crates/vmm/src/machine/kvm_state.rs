//! Same-host KVM state; capture with vCPUs parked and interrupt producers quiesced.

use std::mem::size_of;

use kvm_bindings::{
    CpuId, Xsave, kvm_clock_data, kvm_debugregs, kvm_irqchip, kvm_lapic_state, kvm_mp_state,
    kvm_msr_entry, kvm_pit_state2, kvm_regs, kvm_sregs, kvm_vcpu_events, kvm_xcrs, kvm_xsave,
    kvm_xsave2,
};
use kvm_ioctls::{Cap, VmFd};
use vmm_sys_util::fam::FamStruct;

use crate::arch::x86_64::msr;

/// XSAVE2 includes permitted dynamic user features, not just non-init XSTATE_BV components.
pub(super) fn xsave_fam_len(vm: &VmFd) -> Option<usize> {
    xsave_fam_len_from_size(vm.check_extension_int(Cap::Xsave2))
}

pub(super) fn xsave_fam_len_from_size(size: i32) -> Option<usize> {
    let extra = usize::try_from(size)
        .ok()?
        .checked_sub(size_of::<kvm_xsave>())?;
    let entry_size = size_of::<<kvm_xsave2 as FamStruct>::Entry>();
    extra
        .is_multiple_of(entry_size)
        .then_some(extra / entry_size)
}

#[derive(Debug, Clone)]
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

/// User XSAVE uses the VM-sized FAM; CET uses CPUID-conditioned MSRs and the active SSP ONE_REG.
#[derive(Debug)]
pub struct VcpuSnapshot {
    pub index: u8,
    pub mp_state: kvm_mp_state,
    pub regs: kvm_regs,
    pub sregs: kvm_sregs,
    pub xsave: Xsave,
    pub lapic: kvm_lapic_state,
    pub cpuid: CpuId,
    pub msrs: Vec<kvm_msr_entry>,
    /// Active SSP is present exactly when CPUID advertises shadow stack support.
    pub ssp: Option<u64>,
    pub xcrs: kvm_xcrs,
    pub debug_regs: kvm_debugregs,
    pub vcpu_events: kvm_vcpu_events,
}

impl VcpuSnapshot {
    pub(super) fn ssp_matches_cpuid(&self) -> bool {
        self.ssp.is_some() == msr::has_shadow_stack(&self.cpuid)
    }

    pub(super) fn xstate_supported(&self) -> bool {
        let region = &self.xsave.as_fam_struct_ref().xsave.region;
        let xstate_bv = u64::from(region[128]) | (u64::from(region[129]) << 32);
        let xcomp_bv = u64::from(region[130]) | (u64::from(region[131]) << 32);
        xcomp_bv == 0 && msr::snapshot_xstate_supported(&self.cpuid, &self.msrs, xstate_bv)
    }
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum CaptureVmError {
    /// capture irqchip {chip}
    Irqchip {
        chip: u32,
        #[source]
        source: kvm_ioctls::Error,
    },
    /// capture the PIT
    Pit(#[source] kvm_ioctls::Error),
    /// capture the VM clock
    Clock(#[source] kvm_ioctls::Error),
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum RestoreVmError {
    /// restore irqchip {chip}
    Irqchip {
        chip: u32,
        #[source]
        source: kvm_ioctls::Error,
    },
    /// restore the PIT
    Pit(#[source] kvm_ioctls::Error),
    /// restore the VM clock
    Clock(#[source] kvm_ioctls::Error),
}

/// Capture after vCPUs: their state reads may accept pending APIC events.
pub(super) fn capture_vm_kvm(vm: &VmFd) -> Result<VmKvmState, CaptureVmError> {
    let chip = |chip_id| {
        let mut state = kvm_irqchip {
            chip_id,
            ..Default::default()
        };
        vm.get_irqchip(&mut state)
            .map_err(|source| CaptureVmError::Irqchip {
                chip: chip_id,
                source,
            })?;
        Ok::<_, CaptureVmError>(state)
    };
    let pic_master = chip(0)?;
    let pic_slave = chip(1)?;
    let ioapic = chip(2)?;
    let pit = vm.get_pit2().map_err(CaptureVmError::Pit)?;
    let clock = vm.get_clock().map_err(CaptureVmError::Clock)?;
    Ok(VmKvmState {
        pic_master,
        pic_slave,
        ioapic,
        pit,
        clock,
    })
}

/// Restore IRQ chips/PIT before vCPUs; install the clock only after vCPU restore.
pub(super) fn restore_vm_kvm(vm: &VmFd, state: &VmKvmState) -> Result<(), RestoreVmError> {
    for chip in [&state.pic_master, &state.pic_slave, &state.ioapic] {
        vm.set_irqchip(chip)
            .map_err(|source| RestoreVmError::Irqchip {
                chip: chip.chip_id,
                source,
            })?;
    }
    vm.set_pit2(&state.pit).map_err(RestoreVmError::Pit)
}

/// Install the clock after restoring every vCPU, before anything runs.
pub(super) fn restore_vm_clock(vm: &VmFd, state: &VmKvmState) -> Result<(), RestoreVmError> {
    vm.set_clock(&state.clock).map_err(RestoreVmError::Clock)
}
