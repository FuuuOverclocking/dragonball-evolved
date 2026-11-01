use std::collections::BTreeMap;
use std::sync::Arc;

use kvm_ioctls::Cap;

use super::MachineEvents;
use super::block::{AttachBlockError, BlockSnapshot, CaptureBlockError, PreparedBlock};
use super::build::{BuildError, PreparedDevice, assemble_parked};
use super::cleanup::PendingCleanup;
use super::kvm_state::{self, CaptureVmError, RestoreVmError, VcpuSnapshot, VmKvmState};
use super::mem::MemSnapshot;
use super::plan::{DevicePlan, MachinePlan, PlanError};
use super::platform::{
    ConsoleBackend, PlatformCaptureError, PlatformError, PlatformQuiesced, PlatformRestoreError,
    PlatformSnapshot,
};
use super::state::{Parts, PausedMachine, QuiescedBlock};
use super::vcpu::{self, CaptureVcpuError, RestoreVcpuError};
use crate::arch::x86_64::{cpuid, msr};
use crate::kvm_context::{HostCapabilities, KvmContext};
use crate::memory::policy::HostPolicyError;
use crate::memory::store::MemoryStore;
use crate::vm::{Binding, CreateVmError, KvmVcpuError, KvmVm, MemSlots, create_vm};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceSnapshot {
    Block(BlockSnapshot),
    Mem(MemSnapshot),
}

/// Same-host machine state, separate from the uniquely owned RAM and all host endpoints.
#[derive(Debug)]
pub struct MachineSnapshot {
    pub plan: MachinePlan,
    pub vm: VmKvmState,
    pub vcpus: Vec<VcpuSnapshot>,
    pub platform: PlatformSnapshot,
    pub devices: BTreeMap<String, DeviceSnapshot>,
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum CaptureError {
    /// whole-machine memory hotplug snapshots are not supported yet; population must be captured with RAM
    MemoryHotplugUnsupported,
    /// capture block device {id}
    Block {
        id: String,
        #[source]
        source: CaptureBlockError,
    },
    /// capture the platform
    Platform(#[from] PlatformCaptureError),
    /// capture the vCPUs
    Vcpus(#[from] CaptureVcpuError),
    /// capture the VM
    Vm(#[from] CaptureVmError),
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum RebuildError {
    /// rebuilding a machine needs a tokio runtime on this thread
    NoRuntime,
    /// whole-machine memory hotplug rebuild is not supported yet; empty population cannot replace captured state
    MemoryHotplugUnsupported,
    /// replay the recorded machine plan
    Plan(#[from] PlanError),
    /// supplied RAM does not have the recorded layout
    MemoryLayout,
    /// supplied RAM does not fit this host
    MemoryPolicy(#[from] HostPolicyError),
    /// saved vCPU count {saved} differs from the plan count {planned}
    VcpuCount { saved: usize, planned: u8 },
    /// saved vCPU at position {position} has index {index}
    VcpuIndex { position: usize, index: u8 },
    /// derive the current host's CPUID for vCPU {index}
    HostCpuid {
        index: u8,
        #[source]
        source: cpuid::CpuidError,
    },
    /// saved CPUID of vCPU {0} does not match this host and topology
    CpuHost(u8),
    /// saved MSR layout of vCPU {0} does not match the snapshot contract
    CpuMsrs(u8),
    /// saved active SSP presence of vCPU {0} does not match CPUID shadow stack support
    CpuSsp(u8),
    /// host does not report a valid KVM_CAP_XSAVE2 buffer size: {0}
    HostXsaveSize(i32),
    /// saved XSAVE FAM of vCPU {index} has {saved} words, host permissions require {required}
    CpuXsaveSize {
        index: u8,
        saved: usize,
        required: usize,
    },
    /// saved XCR layout of vCPU {0} is invalid
    CpuXcrs(u8),
    /// saved vCPU {0} has unsupported supervisor or inconsistent XSAVE state
    CpuXstate(u8),
    /// saved irqchip identities must be PIC master, PIC slave and IOAPIC
    Irqchips,
    /// snapshot has no state for device {0}
    MissingDevice(String),
    /// snapshot has state for unplanned device {0}
    ExtraDevice(String),
    /// snapshot device {0} has the wrong kind for its plan
    DeviceKind(String),
    /// validate saved platform state
    PlatformState(#[from] PlatformRestoreError),
    /// prepare or signal a block device
    Block(#[from] AttachBlockError),
    /// create the VM
    Vm(#[from] CreateVmError),
    /// restore the VM
    VmState(#[from] RestoreVmError),
    /// create a vCPU
    CreateVcpu(#[from] KvmVcpuError),
    /// restore a vCPU
    VcpuState(#[from] RestoreVcpuError),
    /// assemble the parked machine
    Assembly(#[source] BuildError),
    /// verify restored device startup acknowledgements
    Quiesce(#[source] CaptureError),
    /// reassert pending platform interrupts
    Platform(#[from] PlatformError),
}

#[derive(Debug)]
pub enum RebuildMemory {
    Ready(MemoryStore),
    Pending(PendingCleanup),
}

/// rebuild the machine
#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub struct RebuildFailure {
    #[source]
    pub error: RebuildError,
    pub memory: RebuildMemory,
}

impl RebuildFailure {
    fn ready(error: RebuildError, memory: MemoryStore) -> Self {
        Self {
            error,
            memory: RebuildMemory::Ready(memory),
        }
    }

    fn pending(error: RebuildError, pending: PendingCleanup) -> Self {
        Self {
            error,
            memory: RebuildMemory::Pending(pending),
        }
    }

    fn unstarted(
        error: RebuildError,
        vm: Arc<KvmVm>,
        bindings: Vec<Binding>,
        slots: MemSlots,
    ) -> Self {
        drop(bindings);
        drop(slots);
        match KvmVm::into_memory(vm) {
            Ok(memory) => Self::ready(error, memory),
            Err(vm) => Self::pending(error, PendingCleanup::vm(vm)),
        }
    }
}

fn capture_devices(
    parts: &Parts,
    blocks: &BTreeMap<String, QuiescedBlock>,
) -> Result<BTreeMap<String, DeviceSnapshot>, CaptureError> {
    if parts.plan.memory_hotplug().is_some() || !parts.mems.is_empty() {
        return Err(CaptureError::MemoryHotplugUnsupported);
    }
    parts
        .blocks
        .iter()
        .map(|(id, block)| {
            block
                .capture(&blocks[id])
                .map(|state| (id.clone(), DeviceSnapshot::Block(state)))
                .map_err(|source| CaptureError::Block {
                    id: id.clone(),
                    source,
                })
        })
        .collect()
}

fn capture_platform(
    parts: &Parts,
    platform: &PlatformQuiesced,
) -> Result<PlatformSnapshot, CaptureError> {
    parts
        .platform
        .capture(platform)
        .map_err(CaptureError::Platform)
}

impl PausedMachine {
    /// Fresh quiesce replies prove worker liveness and retain pending input while CPUs stay parked.
    pub async fn capture(&self) -> Result<MachineSnapshot, CaptureError> {
        if self.parts.plan.memory_hotplug().is_some() || !self.parts.mems.is_empty() {
            return Err(CaptureError::MemoryHotplugUnsupported);
        }
        let (quiesced_platform, blocks, _, _) = self.parts.quiesce_devices().await;
        let devices = capture_devices(self.parts.as_ref(), &blocks)?;
        let platform = capture_platform(self.parts.as_ref(), &quiesced_platform)?;
        let vcpus = self.parts.vcpus.capture_state(self.parts.vm.fd())?;
        let vm = kvm_state::capture_vm_kvm(self.parts.vm.fd())?;
        // Refuse a guest-visible worker that exited while KVM state was read.
        capture_devices(self.parts.as_ref(), &blocks)?;
        capture_platform(self.parts.as_ref(), &quiesced_platform)?;
        Ok(MachineSnapshot {
            plan: self.parts.plan.clone(),
            vm,
            vcpus,
            platform,
            devices,
        })
    }
}

fn preflight(
    snapshot: &MachineSnapshot,
    memory: &MemoryStore,
    host: &HostCapabilities,
) -> Result<(), RebuildError> {
    let plan = &snapshot.plan;
    if plan.memory_hotplug().is_some() {
        return Err(RebuildError::MemoryHotplugUnsupported);
    }
    plan.replay(host)?;
    if memory.layout() != plan.memory() {
        return Err(RebuildError::MemoryLayout);
    }
    memory.policy().check(memory.layout(), host)?;
    if snapshot.vcpus.len() != usize::from(plan.vcpus()) {
        return Err(RebuildError::VcpuCount {
            saved: snapshot.vcpus.len(),
            planned: plan.vcpus(),
        });
    }
    for (position, cpu) in snapshot.vcpus.iter().enumerate() {
        if usize::from(cpu.index) != position {
            return Err(RebuildError::VcpuIndex {
                position,
                index: cpu.index,
            });
        }
        validate_cpu(cpu, plan.vcpus(), host)?;
    }
    if (
        snapshot.vm.pic_master.chip_id,
        snapshot.vm.pic_slave.chip_id,
        snapshot.vm.ioapic.chip_id,
    ) != (0, 1, 2)
    {
        return Err(RebuildError::Irqchips);
    }
    for (id, device) in plan.devices() {
        match (device, snapshot.devices.get(id)) {
            (_, None) => return Err(RebuildError::MissingDevice(id.clone())),
            (DevicePlan::Block(_), Some(DeviceSnapshot::Block(_))) => {}
            (DevicePlan::Mem(_), _) => return Err(RebuildError::MemoryHotplugUnsupported),
            _ => return Err(RebuildError::DeviceKind(id.clone())),
        }
    }
    for id in snapshot.devices.keys() {
        if !plan.devices().contains_key(id) {
            return Err(RebuildError::ExtraDevice(id.clone()));
        }
    }
    snapshot.platform.validate(plan.has_com1())?;
    Ok(())
}

/// Match the permission-sized capture contract; target CPUID allocation is still checked during restore.
fn preflight_xsave(snapshot: &MachineSnapshot, host_size: i32) -> Result<(), RebuildError> {
    let required = kvm_state::xsave_fam_len_from_size(host_size)
        .ok_or(RebuildError::HostXsaveSize(host_size))?;
    for cpu in &snapshot.vcpus {
        let saved = cpu.xsave.as_slice().len();
        if saved != required {
            return Err(RebuildError::CpuXsaveSize {
                index: cpu.index,
                saved,
                required,
            });
        }
    }
    Ok(())
}

fn validate_cpu(
    cpu: &VcpuSnapshot,
    count: u8,
    host: &HostCapabilities,
) -> Result<(), RebuildError> {
    let expected = cpuid::normalize(&host.supported_cpuid, &host.host_cpuid, count, cpu.index)
        .map_err(|source| RebuildError::HostCpuid {
            index: cpu.index,
            source,
        })?;
    let mut saved = cpu.cpuid.as_slice().to_vec();
    let mut expected = expected.as_slice().to_vec();
    // KVM owns these runtime bits and XSAVE lengths (arch/x86/kvm/cpuid.c).
    for entry in saved.iter_mut().chain(expected.iter_mut()) {
        match (entry.function, entry.index) {
            (1, 0) => {
                entry.ecx &= !((1 << 27) | (1 << 3));
                entry.edx &= !(1 << 9);
            }
            (7, 0) => entry.ecx &= !(1 << 4),
            (0xd, 0 | 1) => entry.ebx = 0,
            _ => {}
        }
    }
    if saved != expected {
        return Err(RebuildError::CpuHost(cpu.index));
    }
    if !msr::snapshot_msrs_match(&cpu.cpuid, &cpu.msrs) {
        return Err(RebuildError::CpuMsrs(cpu.index));
    }
    if !cpu.ssp_matches_cpuid() {
        return Err(RebuildError::CpuSsp(cpu.index));
    }
    if !cpu.xstate_supported() {
        return Err(RebuildError::CpuXstate(cpu.index));
    }
    let nr = cpu.xcrs.nr_xcrs as usize;
    if nr > cpu.xcrs.xcrs.len()
        || cpu.xcrs.xcrs[..nr]
            .iter()
            .enumerate()
            .any(|(position, xcr)| xcr.xcr != 0 || position != 0)
    {
        return Err(RebuildError::CpuXcrs(cpu.index));
    }
    Ok(())
}

/// Drive to completion without cancellation; CPUs/devices stay parked until explicit resume.
pub async fn rebuild(
    snapshot: MachineSnapshot,
    memory: MemoryStore,
    ctx: &KvmContext,
) -> Result<(PausedMachine, MachineEvents), RebuildFailure> {
    rebuild_with(snapshot, memory, ctx, ConsoleBackend::Stdio).await
}

pub(super) async fn rebuild_with(
    snapshot: MachineSnapshot,
    memory: MemoryStore,
    ctx: &KvmContext,
    console: ConsoleBackend,
) -> Result<(PausedMachine, MachineEvents), RebuildFailure> {
    if tokio::runtime::Handle::try_current().is_err() {
        return Err(RebuildFailure::ready(RebuildError::NoRuntime, memory));
    }
    let validated = preflight(&snapshot, &memory, ctx.capabilities()).and_then(|()| {
        // Linux's no-VM XSAVE2 query uses current guest-group permissions; they must stay unchanged.
        preflight_xsave(&snapshot, ctx.kvm().check_extension_int(Cap::Xsave2))
    });
    if let Err(error) = validated {
        return Err(RebuildFailure::ready(error, memory));
    }
    let MachineSnapshot {
        plan,
        vm: state,
        vcpus: states,
        platform,
        devices,
    } = snapshot;
    let prepared = devices
        .into_iter()
        .map(|(id, state)| match (&plan.devices()[&id], state) {
            (DevicePlan::Block(block), DeviceSnapshot::Block(state)) => {
                PreparedBlock::restore(&id, block, memory.view(), state)
                    .map(|prepared| (id, PreparedDevice::Block(prepared)))
                    .map_err(RebuildError::Block)
            }
            (DevicePlan::Mem(_), _) => Err(RebuildError::MemoryHotplugUnsupported),
            _ => Err(RebuildError::DeviceKind(id)),
        })
        .collect::<Result<BTreeMap<_, _>, _>>();
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return Err(RebuildFailure::ready(error, memory)),
    };
    let (vm, bindings, slots) = match create_vm(ctx, memory) {
        Ok(created) => created,
        Err((memory, error)) => {
            drop(prepared);
            return Err(RebuildFailure::ready(RebuildError::Vm(error), memory));
        }
    };
    let restored = (|| {
        kvm_state::restore_vm_kvm(vm.fd(), &state)?;
        let mut vcpus = Vec::with_capacity(states.len());
        for state in &states {
            let cpu = vm.create_vcpu(state.index)?;
            vcpu::restore_state(&cpu, vm.fd(), state)?;
            vcpus.push(cpu);
        }
        kvm_state::restore_vm_clock(vm.fd(), &state)?;
        for (cpu, state) in vcpus.iter().zip(&states) {
            vcpu::restore_wall_clock(cpu, state)?;
        }
        Ok::<_, RebuildError>(vcpus)
    })();
    let vcpus = match restored {
        Ok(vcpus) => vcpus,
        Err(error) => {
            drop(prepared);
            return Err(RebuildFailure::unstarted(error, vm, bindings, slots));
        }
    };
    let (mut paused, events) = assemble_parked(
        plan, vm, bindings, slots, vcpus, prepared, &platform, console,
    )
    .map_err(|(pending, error)| RebuildFailure::pending(RebuildError::Assembly(error), pending))?;
    let (platform, blocks, mems, memory_services) = paused.parts.quiesce_devices().await;
    let verified = capture_devices(paused.parts.as_ref(), &blocks)
        .and_then(|_| capture_platform(paused.parts.as_ref(), &platform));
    if let Err(error) = verified {
        return Err(RebuildFailure::pending(
            RebuildError::Quiesce(error),
            paused.into_cleanup(),
        ));
    }
    paused.record_quiesced(platform, blocks, mems, memory_services);
    for block in paused.parts.blocks.values() {
        if let Err(error) = block.reraise_pending() {
            return Err(RebuildFailure::pending(
                RebuildError::Block(error),
                paused.into_cleanup(),
            ));
        }
    }
    if let Err(error) = paused.parts.platform.reraise_pending() {
        return Err(RebuildFailure::pending(
            RebuildError::Platform(error),
            paused.into_cleanup(),
        ));
    }
    Ok((paused, events))
}

#[cfg(test)]
mod tests {
    use api::RawVmmOp;
    use api::block::{AddDisk, BlockDevConfig};
    use api::boot::{KernelConfig, SetKernel};
    use kvm_bindings::{CpuId, kvm_cpuid_entry2, kvm_irqchip, kvm_msr_entry};

    use super::*;
    use crate::align::BackingPageSize;
    use crate::devices::virtio::config::ConfigSnapshot;
    use crate::devices::virtio::mmio::{DeviceDesc, MmioRegs, MmioTransportSnapshot, VERSION_1};
    use crate::memory::layout::MemoryLayout;
    use crate::memory::policy::{BackingKind, HostPolicy};
    use crate::spec::MachineSpec;

    fn host() -> HostCapabilities {
        let mut host = HostCapabilities::fake(46, 32, 4096);
        let entries: Vec<_> = [
            0,
            1,
            4,
            6,
            7,
            0xa,
            0xb,
            0x1f,
            0x8000_0001,
            0x8000_0005,
            0x8000_0006,
            0x8000_0008,
        ]
        .into_iter()
        .map(|function| kvm_cpuid_entry2 {
            function,
            eax: if function == 0x8000_0008 { 46 } else { 0 },
            ..Default::default()
        })
        .collect();
        host.supported_cpuid = CpuId::from_entries(&entries).unwrap();
        host
    }

    fn fixture(kind: BackingKind) -> (MachineSnapshot, MemoryStore, HostCapabilities) {
        let host = host();
        let spec = MachineSpec::default()
            .apply(
                SetKernel(KernelConfig {
                    path: "/never-opened/kernel".into(),
                    cmdline: None,
                    initrd: None,
                })
                .into_vmm_command(),
            )
            .unwrap()
            .apply(
                AddDisk(BlockDevConfig {
                    id: Some("root".into()),
                    path: "/never-opened/disk".into(),
                    read_only: true,
                    root: true,
                })
                .into_vmm_command(),
            )
            .unwrap();
        let (plan, _) = MachinePlan::new(&spec, &host).unwrap();
        let memory = MemoryStore::materialize(
            plan.memory().clone(),
            HostPolicy {
                kind,
                page_size: BackingPageSize::base(host.page_size),
            },
            &host,
        )
        .unwrap();
        let cpuid = cpuid::normalize(&host.supported_cpuid, &host.host_cpuid, 1, 0).unwrap();
        let msrs = msr::snapshot_msr_indices(&cpuid)
            .into_iter()
            .map(|index| kvm_msr_entry {
                index,
                ..Default::default()
            })
            .collect();
        let cpu = VcpuSnapshot {
            index: 0,
            mp_state: Default::default(),
            regs: Default::default(),
            sregs: Default::default(),
            xsave: kvm_bindings::Xsave::new(0).unwrap(),
            lapic: Default::default(),
            cpuid,
            msrs,
            ssp: None,
            xcrs: Default::default(),
            debug_regs: Default::default(),
            vcpu_events: Default::default(),
        };
        let chip = |chip_id| kvm_irqchip {
            chip_id,
            ..Default::default()
        };
        let desc = DeviceDesc {
            device_type: 2,
            features: VERSION_1,
            required_features: VERSION_1,
            queue_max: vec![16],
            config: vec![],
        };
        let block = DeviceSnapshot::Block(BlockSnapshot {
            transport: MmioTransportSnapshot {
                regs: MmioRegs::new(&desc),
                config: ConfigSnapshot {
                    bytes: desc.config.clone(),
                    generation: 0,
                },
                desc,
                interrupt_status: 0,
            },
            queue: None,
        });
        let saved = MachineSnapshot {
            platform: PlatformSnapshot {
                i8042: Default::default(),
                serial: Some(Default::default()),
            },
            plan,
            vm: VmKvmState {
                pic_master: chip(0),
                pic_slave: chip(1),
                ioapic: chip(2),
                pit: Default::default(),
                clock: Default::default(),
            },
            vcpus: vec![cpu],
            devices: BTreeMap::from([("root".into(), block)]),
        };
        (saved, memory, host)
    }

    #[test]
    fn block_preflight_rejects_memory_device_state_before_opening_backends() {
        for kind in [BackingKind::AnonymousPrivate, BackingKind::SharedMemfd] {
            let (mut saved, memory, host) = fixture(kind);
            let DeviceSnapshot::Block(block) = saved.devices.remove("root").unwrap() else {
                panic!("fixture root is not a block device")
            };
            saved.devices.insert(
                "root".into(),
                DeviceSnapshot::Mem(MemSnapshot {
                    transport: block.transport,
                    state: crate::devices::virtio::mem::MemQuiesced {
                        requested_size: 0,
                        usable_region_size: 0,
                        memory_revision: 0,
                        queue: None,
                    },
                }),
            );
            assert!(matches!(preflight(&saved, &memory, &host),
                Err(RebuildError::DeviceKind(id)) if id == "root"
            ));
        }
    }

    #[test]
    fn hotplug_rebuild_preflight_refuses_without_creating_a_vm_or_losing_original_ram() {
        use api::machine::{MemoryHotplugConfig, SetMemoryHotplug};

        for kind in [BackingKind::AnonymousPrivate, BackingKind::SharedMemfd] {
            let (mut saved, boot, host) = fixture(kind);
            let spec = MachineSpec::default()
                .apply(
                    SetKernel(KernelConfig {
                        path: "/never-opened/kernel".into(),
                        cmdline: None,
                        initrd: None,
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
            saved.plan = MachinePlan::new(&spec, &host).unwrap().0;
            let policy = *boot.policy();
            drop(boot);
            let memory =
                MemoryStore::materialize(saved.plan.memory().clone(), policy, &host).unwrap();
            let weak = Arc::downgrade(memory.view());
            saved.vcpus.clear();
            saved.devices.clear();
            let error = preflight(&saved, &memory, &host).unwrap_err();
            assert!(matches!(error, RebuildError::MemoryHotplugUnsupported));
            let failure = RebuildFailure::ready(error, memory);
            let RebuildMemory::Ready(memory) = failure.memory else {
                panic!("unsupported memory snapshot retained a VM")
            };
            assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), memory.view()));
            assert_eq!(memory.policy().kind, kind);
            drop(memory);
            assert!(weak.upgrade().is_none());
        }
    }

    #[test]
    fn preflight_requires_clock_aliases_and_their_captured_order_even_when_disabled() {
        let (mut saved, memory, mut host) = fixture(BackingKind::AnonymousPrivate);
        let mut supported = host.supported_cpuid.as_slice().to_vec();
        supported.push(kvm_cpuid_entry2 {
            function: 0x4000_0001,
            eax: (1 << 0) | (1 << 3), // CLOCKSOURCE and CLOCKSOURCE2
            ..Default::default()
        });
        host.supported_cpuid = CpuId::from_entries(&supported).unwrap();
        let cpu = &mut saved.vcpus[0];
        cpu.cpuid = cpuid::normalize(&host.supported_cpuid, &host.host_cpuid, 1, 0).unwrap();
        cpu.msrs = msr::snapshot_msr_indices(&cpu.cpuid)
            .into_iter()
            .map(|index| kvm_msr_entry {
                index,
                ..Default::default()
            })
            .collect();
        for enabled in [false, true] {
            for entry in &mut cpu.msrs {
                entry.data = match entry.index {
                    msr::MSR_KVM_SYSTEM_TIME | msr::MSR_KVM_SYSTEM_TIME_NEW => {
                        0x2000 | u64::from(enabled)
                    }
                    msr::MSR_KVM_WALL_CLOCK | msr::MSR_KVM_WALL_CLOCK_NEW => 0x3000,
                    _ => 0,
                };
            }
            assert!(validate_cpu(cpu, 1, &host).is_ok());
            let last = cpu.msrs.pop().unwrap();
            assert!(matches!(
                validate_cpu(cpu, 1, &host),
                Err(RebuildError::CpuMsrs(0))
            ));
            cpu.msrs.push(last);
            cpu.msrs.swap(0, 1);
            assert!(matches!(
                validate_cpu(cpu, 1, &host),
                Err(RebuildError::CpuMsrs(0))
            ));
            cpu.msrs.swap(0, 1);
            let alias = cpu
                .msrs
                .iter()
                .position(|entry| entry.index == msr::MSR_KVM_SYSTEM_TIME_NEW)
                .unwrap();
            cpu.msrs[alias].data ^= 1;
            assert!(matches!(
                validate_cpu(cpu, 1, &host),
                Err(RebuildError::CpuMsrs(0))
            ));
            cpu.msrs[alias].data ^= 1;
        }
        assert!(preflight(&saved, &memory, &host).is_ok());
    }

    #[test]
    fn preflight_requires_active_ssp_exactly_for_shadow_stacks() {
        for (shstk, ibt) in [(false, false), (false, true), (true, false), (true, true)] {
            let (mut saved, memory, mut host) = fixture(BackingKind::AnonymousPrivate);
            let mut entries = host.supported_cpuid.as_slice().to_vec();
            let features = entries
                .iter_mut()
                .find(|entry| entry.function == 7)
                .unwrap();
            features.ecx |= u32::from(shstk) << 7;
            features.edx |= u32::from(ibt) << 20;
            host.supported_cpuid = CpuId::from_entries(&entries).unwrap();
            let cpu = &mut saved.vcpus[0];
            cpu.cpuid = cpuid::normalize(&host.supported_cpuid, &host.host_cpuid, 1, 0).unwrap();
            cpu.msrs = msr::snapshot_msr_indices(&cpu.cpuid)
                .into_iter()
                .map(|index| kvm_msr_entry {
                    index,
                    ..Default::default()
                })
                .collect();
            cpu.ssp = shstk.then_some(0);
            assert!(validate_cpu(cpu, 1, &host).is_ok());
            cpu.ssp = (!shstk).then_some(0);
            assert!(matches!(
                validate_cpu(cpu, 1, &host),
                Err(RebuildError::CpuSsp(0))
            ));
            cpu.ssp = shstk.then_some(0x2000);
            assert!(preflight(&saved, &memory, &host).is_ok());
        }
    }

    #[test]
    fn xsave_length_preflight_is_strict_without_creating_a_vm() {
        let (mut saved, _memory, _host) = fixture(BackingKind::AnonymousPrivate);
        for host_size in [-1, 0, 4095, 4097] {
            assert!(matches!(
                preflight_xsave(&saved, host_size),
                Err(RebuildError::HostXsaveSize(size)) if size == host_size
            ));
        }
        assert!(preflight_xsave(&saved, 4096).is_ok());
        assert!(matches!(
            preflight_xsave(&saved, 4100),
            Err(RebuildError::CpuXsaveSize {
                index: 0,
                saved: 0,
                required: 1
            })
        ));
        saved.vcpus[0].xsave = kvm_bindings::Xsave::new(1).unwrap();
        assert!(matches!(
            preflight_xsave(&saved, 4096),
            Err(RebuildError::CpuXsaveSize {
                index: 0,
                saved: 1,
                required: 0
            })
        ));
        assert!(preflight_xsave(&saved, 4100).is_ok());
    }

    #[test]
    fn preflight_rejects_shapes_without_a_kvm_context_or_opening_paths() {
        for kind in [BackingKind::AnonymousPrivate, BackingKind::SharedMemfd] {
            let (mut saved, memory, host) = fixture(kind);
            assert!(preflight(&saved, &memory, &host).is_ok());
            let root = saved.devices.remove("root").unwrap();
            assert!(
                matches!(preflight(&saved, &memory, &host), Err(RebuildError::MissingDevice(id)) if id == "root")
            );
            saved.devices.insert("root".into(), root.clone());
            saved.devices.insert("extra".into(), root);
            assert!(
                matches!(preflight(&saved, &memory, &host), Err(RebuildError::ExtraDevice(id)) if id == "extra")
            );
            saved.devices.remove("extra");
            saved.vcpus[0].index = 1;
            assert!(matches!(
                preflight(&saved, &memory, &host),
                Err(RebuildError::VcpuIndex { .. })
            ));
            saved.vcpus[0].index = 0;
            let cpu = saved.vcpus.pop().unwrap();
            assert!(matches!(
                preflight(&saved, &memory, &host),
                Err(RebuildError::VcpuCount { .. })
            ));
            saved.vcpus.push(cpu);
            saved.platform.serial = None;
            assert!(matches!(
                preflight(&saved, &memory, &host),
                Err(RebuildError::PlatformState(PlatformRestoreError::Com1))
            ));
            saved.platform.serial = Some(Default::default());
            saved.vm.ioapic.chip_id = 0;
            assert!(matches!(
                preflight(&saved, &memory, &host),
                Err(RebuildError::Irqchips)
            ));
            saved.vm.ioapic.chip_id = 2;
            let wrong = MemoryStore::materialize(
                MemoryLayout::boot(32 << 20).unwrap(),
                *memory.policy(),
                &host,
            )
            .unwrap();
            assert!(matches!(
                preflight(&saved, &wrong, &host),
                Err(RebuildError::MemoryLayout)
            ));
            let mut limited = host.clone();
            limited.max_vcpus = 0;
            assert!(matches!(
                preflight(&saved, &memory, &limited),
                Err(RebuildError::Plan(PlanError::VcpuCount { .. }))
            ));
            let mut foreign = host.clone();
            foreign.host_cpuid.extended_l1.eax ^= 1;
            assert!(matches!(
                preflight(&saved, &memory, &foreign),
                Err(RebuildError::CpuHost(0))
            ));
            let weak = Arc::downgrade(memory.view());
            let failure = RebuildFailure::ready(RebuildError::MemoryLayout, memory);
            let RebuildMemory::Ready(memory) = failure.memory else {
                panic!("preflight retained a VM")
            };
            assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), memory.view()));
            assert_eq!(memory.policy().kind, kind);
        }
    }
}
