//! Capturing a paused machine as data and restoring a machine from it.
//!
//! A snapshot holds the machine's plan and the KVM, vCPU and device state. It
//! holds no live kernel objects and no guest RAM: a restore takes over the RAM
//! of the torn-down machine, creates new kernel objects and attaches every
//! device at the grant the plan recorded.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use kvm_bindings::{
    CpuId, kvm_clock_data, kvm_debugregs, kvm_irqchip, kvm_lapic_state, kvm_mp_state,
    kvm_msr_entry, kvm_pit_state2, kvm_regs, kvm_sregs, kvm_vcpu_events, kvm_xcrs, kvm_xsave,
};
use kvm_ioctls::VmFd;

use super::block::BlockSnapshot;
use super::build::{self, AttachedDevice, Machine, Restore};
use super::memory::GuestRam;
use super::plan::{self, DevicePlan, MachinePlan};
use super::platform::PlatformSnapshot;
use super::vcpu::{self, Vcpus};
use super::vm::KvmVm;
use crate::KvmContext;

pub struct MachineSnapshot {
    pub plan: MachinePlan,
    pub vm: VmKvmState,
    pub vcpus: Vec<VcpuSnapshot>,
    pub platform: PlatformSnapshot,
    /// In plan order.
    pub devices: Vec<DeviceSnapshot>,
}

impl MachineSnapshot {
    /// The captured state of the disk `id`.
    pub fn block(&self, id: &str) -> Option<&BlockSnapshot> {
        self.devices.iter().find_map(|device| match device {
            DeviceSnapshot::Block(block) if block.id == id => Some(block),
            _ => None,
        })
    }
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

/// One device's captured state, by kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceSnapshot {
    Block(BlockSnapshot),
}

impl DeviceSnapshot {
    pub fn id(&self) -> &str {
        match self {
            Self::Block(block) => &block.id,
        }
    }
}

// ---------------------------------------------------------------------------
// Capture
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

/// Capture a paused machine. A device that still runs is refused, since its
/// state moves on, and so is one that has exited: there is no state to capture
/// for it, and restoring it would hide the failure.
pub fn capture(machine: &Machine) -> Result<MachineSnapshot> {
    // Devices first: an interrupt a device raised while finishing its last
    // request must be part of the interrupt controller state captured below.
    let devices = machine
        .devices
        .iter()
        .map(AttachedDevice::capture)
        .collect::<Result<_>>()?;
    let platform = machine.platform.capture()?;
    let vcpus = capture_vcpus(machine.vcpus.as_ref().context("machine has no vCPUs")?)?;
    let vm = capture_vm_kvm(machine.vm.fd())?;
    Ok(MachineSnapshot {
        plan: machine.plan.clone(),
        vm,
        vcpus,
        platform,
        devices,
    })
}

// ---------------------------------------------------------------------------
// Rebuild
// ---------------------------------------------------------------------------

/// Reconstruct a [`Machine`] from a [`MachineSnapshot`] on the guest RAM it was
/// captured with, attaching every device at the grant its plan recorded.
///
/// The returned machine is paused: its vCPUs wait for a resume, and a disk whose
/// backend had a session when captured is reactivated with its captured queue.
///
/// The RAM is taken by value, so a restore needs the RAM of a torn-down machine,
/// and the same RAM cannot back two restores:
///
/// ```compile_fail,E0382
/// use vmm::KvmContext;
/// use vmm::machine::memory::GuestRam;
/// use vmm::machine::snapshot::{MachineSnapshot, rebuild};
///
/// fn restore_twice(a: MachineSnapshot, b: MachineSnapshot, ram: GuestRam, context: &KvmContext) {
///     let _first = rebuild(a, ram, context);
///     let _second = rebuild(b, ram, context);
/// }
/// ```
pub fn rebuild(snapshot: MachineSnapshot, ram: GuestRam, context: &KvmContext) -> Result<Machine> {
    let MachineSnapshot {
        plan,
        vm: vm_state,
        vcpus: vcpu_states,
        platform,
        devices,
    } = snapshot;
    // Rejects grants that overlap or leave their pools before anything is attached.
    plan::replay(&plan.devices)?;
    let restore = Restore {
        platform,
        devices: match_devices(&plan.devices, devices)?,
    };
    if vcpu_states.len() != usize::from(plan.spec.cpus)
        || vcpu_states
            .iter()
            .zip(0..)
            .any(|(vcpu, index)| vcpu.index != index)
    {
        bail!(
            "the snapshot does not hold vCPUs 0..{} in order",
            plan.spec.cpus
        );
    }
    if ram.plan() != &plan.memory {
        bail!("the guest RAM was not mapped from the snapshot's plan");
    }

    let vm = KvmVm::new(context, ram)?;
    let fd = vm.fd();
    fd.set_irqchip(&vm_state.pic_master)
        .context("restore PIC master")?;
    fd.set_irqchip(&vm_state.pic_slave)
        .context("restore PIC slave")?;
    fd.set_irqchip(&vm_state.ioapic).context("restore IOAPIC")?;
    fd.set_pit2(&vm_state.pit).context("restore PIT2")?;
    let mut vcpus = Vec::with_capacity(vcpu_states.len());
    for state in &vcpu_states {
        let vcpu = vm.create_vcpu(state.index)?;
        vcpu::restore_state(&vcpu, state)?;
        vcpus.push(vcpu);
    }
    // After the vCPUs are restored, before anything runs.
    fd.set_clock(&vm_state.clock).context("restore VM clock")?;

    build::assemble(plan, vm, vcpus, Some(restore))
}

/// Puts the captured device states in plan order, so a restore attaches each
/// planned device with its own state. Every planned device needs exactly one.
fn match_devices(
    plans: &[DevicePlan],
    snapshots: Vec<DeviceSnapshot>,
) -> Result<Vec<DeviceSnapshot>> {
    let mut by_id = BTreeMap::new();
    for snapshot in snapshots {
        let id = snapshot.id().to_owned();
        if by_id.insert(id.clone(), snapshot).is_some() {
            bail!("the snapshot holds device {id} twice");
        }
    }
    let ordered = plans
        .iter()
        .map(|plan| {
            by_id
                .remove(plan.id())
                .with_context(|| format!("the snapshot has no state for device {}", plan.id()))
        })
        .collect::<Result<Vec<_>>>()?;
    if let Some(id) = by_id.keys().next() {
        bail!("the snapshot holds device {id}, which the plan does not");
    }
    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::KvmContext;
    use crate::arch::x86_64::layout::ram_regions;
    use crate::devices::virtio::mmio::{DeviceDesc, MmioRegs, MmioTransportSnapshot};
    use crate::machine::block::BlockPlan;
    use crate::machine::memory::MemoryPlan;
    use crate::machine::plan::VirtioMmioGrant;
    use crate::resources::{GpaRange, Gsi};

    #[test]
    fn capture_vm_kvm_returns_irqchip_and_pit_on_kvm() {
        if !Path::new("/dev/kvm").exists() {
            return;
        }
        let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
        let plan = MemoryPlan {
            backing: api::machine::MemoryBacking::Anonymous,
            regions: ram_regions(16 << 20).unwrap(),
        };
        let vm = KvmVm::new(&context, GuestRam::map(&plan).unwrap()).unwrap();

        let state = capture_vm_kvm(vm.fd()).unwrap();
        assert_eq!(state.pic_master.chip_id, 0);
        assert_eq!(state.pic_slave.chip_id, 1);
        assert_eq!(state.ioapic.chip_id, 2);

        drop(vm);
    }

    fn planned(id: &str, slot: u64) -> DevicePlan {
        let start = 0xc000_0000 + slot * 0x1000;
        DevicePlan::Block(BlockPlan {
            id: id.into(),
            path: format!("/{id}").into(),
            read_only: false,
            root: false,
            grant: VirtioMmioGrant {
                mmio: GpaRange::new(start, start + 0x1000),
                gsi: Gsi(5 + slot as u32),
            },
        })
    }

    fn captured(id: &str) -> DeviceSnapshot {
        DeviceSnapshot::Block(BlockSnapshot {
            id: id.into(),
            transport: MmioTransportSnapshot {
                desc: DeviceDesc {
                    device_type: 2,
                    features: 0,
                    queue_max: vec![256],
                    config: Vec::new(),
                },
                regs: MmioRegs::default(),
                interrupt_status: 0,
            },
            queue: None,
        })
    }

    fn ids(snapshots: &[DeviceSnapshot]) -> Vec<&str> {
        snapshots.iter().map(DeviceSnapshot::id).collect()
    }

    #[test]
    fn device_states_are_matched_to_the_plan_by_id() {
        let plans = [planned("root", 0), planned("data", 1)];
        let ordered = match_devices(&plans, vec![captured("data"), captured("root")]).unwrap();
        assert_eq!(ids(&ordered), ["root", "data"]);

        let error = match_devices(&plans, vec![captured("root")]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the snapshot has no state for device data"
        );
        let error = match_devices(
            &plans,
            vec![captured("root"), captured("data"), captured("root")],
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "the snapshot holds device root twice");
        let error = match_devices(
            &plans,
            vec![captured("root"), captured("data"), captured("extra")],
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "the snapshot holds device extra, which the plan does not"
        );
    }
}
