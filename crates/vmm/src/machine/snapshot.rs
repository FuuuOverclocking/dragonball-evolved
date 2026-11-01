use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use kvm_ioctls::Cap;

use super::MachineEvents;
use super::block::{AttachBlockError, BlockSnapshot, CaptureBlockError, PreparedBlock};
use super::build::{BuildError, PreparedDevice, assemble_parked, retain_prepared_memory};
use super::cleanup::PendingCleanup;
use super::kvm_state::{self, CaptureVmError, RestoreVmError, VcpuSnapshot, VmKvmState};
use super::mem::{AttachMemError, CaptureMemError, MemSnapshot, PreparedMem, RestoreMemError};
use super::plan::{DevicePlan, MachinePlan, PlanError};
use super::platform::{
    ConsoleBackend, PlatformCaptureError, PlatformError, PlatformQuiesced, PlatformRestoreError,
    PlatformSnapshot,
};
use super::state::{Parts, PausedMachine, QuiescedBlock, QuiescedMem};
use super::vcpu::{self, CaptureVcpuError, RestoreVcpuError};
use crate::arch::x86_64::{cpuid, msr};
use crate::devices::runtime::Response;
use crate::kvm_context::{HostCapabilities, KvmContext};
use crate::memory::hotplug::{GeometryError, MemGeometry, Population, PopulationError};
use crate::memory::layout::WindowId;
use crate::memory::policy::HostPolicyError;
use crate::memory::service::{MemoryQuiesced, MemoryResources};
use crate::memory::store::MemoryStore;
use crate::vm::{Binding, CreateVmError, KvmVcpuError, KvmVm, MemSlots, create_vm};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceSnapshot {
    Block(BlockSnapshot),
    Mem(MemSnapshot),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySnapshot {
    pub population: Population,
    pub revision: u64,
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum RestoreMemoryError {
    /// validate the supplied RAM backing against the recorded memory geometry
    BackingGeometry(#[source] GeometryError),
    /// saved population geometry differs from the recorded window
    Geometry,
    /// validate the saved population ranges
    Population(#[from] PopulationError),
    /// saved population is not canonical for the recorded geometry
    Canonical,
}

impl MemorySnapshot {
    pub(super) fn validate(&self, geometry: MemGeometry) -> Result<(), RestoreMemoryError> {
        if self.population.geometry() != geometry {
            return Err(RestoreMemoryError::Geometry);
        }
        let restored = Population::restore(geometry, self.population.plugged_ranges())?;
        if restored != self.population {
            return Err(RestoreMemoryError::Canonical);
        }
        Ok(())
    }
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum CaptureMemoryError {
    /// memory service exited before capture
    ServiceExited,
    /// memory service did not quiesce in time
    ServiceTimedOut,
    /// memory service is not quiesced or has an unfinished change
    NotQuiesced,
    /// memory service state differs from its quiesce acknowledgement
    Changed,
    /// validate the quiesced population
    State(#[from] RestoreMemoryError),
}

/// Same-host machine state, separate from the uniquely owned RAM and all host endpoints.
#[derive(Debug)]
pub struct MachineSnapshot {
    pub plan: MachinePlan,
    pub vm: VmKvmState,
    pub vcpus: Vec<VcpuSnapshot>,
    pub platform: PlatformSnapshot,
    pub devices: BTreeMap<String, DeviceSnapshot>,
    pub memory: BTreeMap<WindowId, MemorySnapshot>,
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum CaptureError {
    /// machine has no live device or acknowledgement for plan ID {0}
    MissingDevice(String),
    /// machine has an unplanned device {0}
    ExtraDevice(String),
    /// machine device {0} has the wrong kind
    DeviceKind(String),
    /// machine has no live memory service or acknowledgement for window {0}
    MissingMemory(WindowId),
    /// machine has an unplanned memory service for window {0}
    ExtraMemory(WindowId),
    /// capture memory device {id}
    Mem {
        id: String,
        #[source]
        source: CaptureMemError,
    },
    /// capture memory population for window {window}
    Memory {
        window: WindowId,
        #[source]
        source: CaptureMemoryError,
    },
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
    /// snapshot has no population for planned memory window {0}
    MissingMemory(WindowId),
    /// snapshot has population for unplanned memory window {0}
    ExtraMemory(WindowId),
    /// validate the population for window {window}
    MemoryState {
        window: WindowId,
        #[source]
        source: RestoreMemoryError,
    },
    /// validate saved memory device {id}
    MemState {
        id: String,
        #[source]
        source: RestoreMemError,
    },
    /// prepare or signal a memory device
    Mem(#[from] AttachMemError),
    /// restored device {0} did not acknowledge its exact saved state
    DeviceAcknowledgement(String),
    /// restored memory window {0} did not acknowledge its exact saved population and revision
    MemoryAcknowledgement(WindowId),
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

    async fn unstarted(
        error: RebuildError,
        vm: Arc<KvmVm>,
        bindings: Vec<Binding>,
        slots: MemSlots,
        memory: BTreeMap<WindowId, MemoryResources>,
    ) -> Self {
        match PendingCleanup::unstarted(vm, bindings, slots, memory)
            .wait(Duration::ZERO)
            .await
        {
            Ok(memory) => Self::ready(error, memory),
            Err((pending, _)) => Self::pending(error, pending),
        }
    }
}

fn capture_devices(
    parts: &Parts,
    blocks: &BTreeMap<String, QuiescedBlock>,
    mems: &BTreeMap<String, QuiescedMem>,
    services: &BTreeMap<WindowId, Response<MemoryQuiesced>>,
) -> Result<
    (
        BTreeMap<String, DeviceSnapshot>,
        BTreeMap<WindowId, MemorySnapshot>,
    ),
    CaptureError,
> {
    let mut devices = BTreeMap::new();
    let mut memory = BTreeMap::new();
    for (id, plan) in parts.plan.devices() {
        let state = match plan {
            DevicePlan::Block(_) => {
                if parts.mems.contains_key(id) || mems.contains_key(id) {
                    return Err(CaptureError::DeviceKind(id.clone()));
                }
                let block = parts
                    .blocks
                    .get(id)
                    .ok_or_else(|| CaptureError::MissingDevice(id.clone()))?;
                let ack = blocks
                    .get(id)
                    .ok_or_else(|| CaptureError::MissingDevice(id.clone()))?;
                DeviceSnapshot::Block(block.capture(ack).map_err(|source| CaptureError::Block {
                    id: id.clone(),
                    source,
                })?)
            }
            DevicePlan::Mem(plan) => {
                if parts.blocks.contains_key(id) || blocks.contains_key(id) {
                    return Err(CaptureError::DeviceKind(id.clone()));
                }
                let mem = parts
                    .mems
                    .get(id)
                    .ok_or_else(|| CaptureError::MissingDevice(id.clone()))?;
                let ack = mems
                    .get(id)
                    .ok_or_else(|| CaptureError::MissingDevice(id.clone()))?;
                let window = plan.window_id();
                let service = parts
                    .memory_services
                    .get(&window)
                    .ok_or(CaptureError::MissingMemory(window))?;
                let service_ack = services
                    .get(&window)
                    .ok_or(CaptureError::MissingMemory(window))?;
                let population = mem
                    .capture_memory(plan, service, service_ack)
                    .map_err(|source| CaptureError::Memory { window, source })?;
                let state = mem
                    .capture(plan, &population, parts.vm.memory().view(), ack)
                    .map_err(|source| CaptureError::Mem {
                        id: id.clone(),
                        source,
                    })?;
                memory.insert(window, population);
                DeviceSnapshot::Mem(state)
            }
        };
        devices.insert(id.clone(), state);
    }
    for id in parts
        .blocks
        .keys()
        .chain(parts.mems.keys())
        .chain(blocks.keys())
        .chain(mems.keys())
    {
        if !parts.plan.devices().contains_key(id) {
            return Err(CaptureError::ExtraDevice(id.clone()));
        }
    }
    for window in parts.memory_services.keys().chain(services.keys()) {
        if !memory.contains_key(window) {
            return Err(CaptureError::ExtraMemory(*window));
        }
    }
    Ok((devices, memory))
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
        let (quiesced_platform, blocks, mems, services) = self.parts.quiesce_devices().await;
        let (devices, memory) = capture_devices(self.parts.as_ref(), &blocks, &mems, &services)?;
        let platform = capture_platform(self.parts.as_ref(), &quiesced_platform)?;
        let vcpus = self.parts.vcpus.capture_state(self.parts.vm.fd())?;
        let vm = kvm_state::capture_vm_kvm(self.parts.vm.fd())?;
        let (checked_devices, checked_memory) =
            capture_devices(self.parts.as_ref(), &blocks, &mems, &services)?;
        for (id, saved) in &devices {
            if matches!(saved, DeviceSnapshot::Mem(_)) && checked_devices.get(id) != Some(saved) {
                return Err(CaptureError::Mem {
                    id: id.clone(),
                    source: CaptureMemError::Changed,
                });
            }
        }
        for (window, saved) in &memory {
            if checked_memory.get(window) != Some(saved) {
                return Err(CaptureError::Memory {
                    window: *window,
                    source: CaptureMemoryError::Changed,
                });
            }
        }
        capture_platform(self.parts.as_ref(), &quiesced_platform)?;
        Ok(MachineSnapshot {
            plan: self.parts.plan.clone(),
            vm,
            vcpus,
            platform,
            devices,
            memory,
        })
    }
}

fn preflight(
    snapshot: &MachineSnapshot,
    memory: &MemoryStore,
    host: &HostCapabilities,
) -> Result<(), RebuildError> {
    let plan = &snapshot.plan;
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
            (DevicePlan::Mem(_), Some(DeviceSnapshot::Mem(_))) => {}
            _ => return Err(RebuildError::DeviceKind(id.clone())),
        }
    }
    for id in snapshot.devices.keys() {
        if !plan.devices().contains_key(id) {
            return Err(RebuildError::ExtraDevice(id.clone()));
        }
    }
    for (id, device) in plan.devices() {
        if let DevicePlan::Mem(plan) = device {
            let window = plan.window_id();
            let population = snapshot
                .memory
                .get(&window)
                .ok_or(RebuildError::MissingMemory(window))?;
            // Check the supplied backing before creating a VM that could write clocks into RAM.
            let geometry = MemGeometry::new(
                memory.layout(),
                window,
                plan.geometry().block_size(),
                plan.geometry().chunk_size(),
                memory.policy().page_size,
            )
            .map_err(RestoreMemoryError::BackingGeometry)
            .map_err(|source| RebuildError::MemoryState { window, source })?;
            population
                .validate(geometry)
                .map_err(|source| RebuildError::MemoryState { window, source })?;
            let DeviceSnapshot::Mem(saved) = &snapshot.devices[id] else {
                unreachable!("device kinds were validated")
            };
            saved
                .validate(plan, population, memory.view())
                .map_err(|source| RebuildError::MemState {
                    id: id.clone(),
                    source,
                })?;
        }
    }
    for window in snapshot.memory.keys() {
        if !plan
            .devices()
            .values()
            .any(|device| device.mem().is_some_and(|mem| mem.window_id() == *window))
        {
            return Err(RebuildError::ExtraMemory(*window));
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
        memory: populations,
    } = snapshot;
    let prepared = devices
        .iter()
        .filter_map(|(id, state)| match (&plan.devices()[id], state) {
            (DevicePlan::Block(block), DeviceSnapshot::Block(state)) => Some(
                PreparedBlock::restore(id, block, memory.view(), state.clone())
                    .map(|prepared| (id.clone(), PreparedDevice::Block(prepared)))
                    .map_err(RebuildError::Block),
            ),
            (DevicePlan::Mem(_), DeviceSnapshot::Mem(_)) => None,
            _ => unreachable!("device kinds were validated"),
        })
        .collect::<Result<BTreeMap<_, _>, _>>();
    let mut prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return Err(RebuildFailure::ready(error, memory)),
    };
    let (vm, bindings, mut slots) = match create_vm(ctx, memory) {
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
            return Err(
                RebuildFailure::unstarted(error, vm, bindings, slots, BTreeMap::new()).await,
            );
        }
    };
    let mut unstarted_memory = BTreeMap::new();
    for (id, device) in plan.devices() {
        if let DevicePlan::Mem(mem) = device {
            let permits = match slots.allocate(mem.geometry().chunk_count() as usize) {
                Ok(permits) => permits,
                Err(source) => {
                    drop(vcpus);
                    retain_prepared_memory(prepared, &mut unstarted_memory);
                    let error = AttachMemError::Slots {
                        id: id.clone(),
                        source,
                    };
                    return Err(RebuildFailure::unstarted(
                        error.into(),
                        vm,
                        bindings,
                        slots,
                        unstarted_memory,
                    )
                    .await);
                }
            };
            let DeviceSnapshot::Mem(saved) = &devices[id] else {
                unreachable!("device kinds were validated")
            };
            match PreparedMem::restore(
                id,
                mem,
                Arc::clone(&vm),
                permits,
                populations[&mem.window_id()].clone(),
                saved.clone(),
            ) {
                Ok(mem) => {
                    prepared.insert(id.clone(), PreparedDevice::Mem(mem));
                }
                Err((resources, error)) => {
                    drop(vcpus);
                    unstarted_memory.insert(mem.window_id(), resources);
                    retain_prepared_memory(prepared, &mut unstarted_memory);
                    return Err(RebuildFailure::unstarted(
                        error.into(),
                        vm,
                        bindings,
                        slots,
                        unstarted_memory,
                    )
                    .await);
                }
            }
        }
    }
    let (mut paused, events) = assemble_parked(
        plan, vm, bindings, slots, vcpus, prepared, &platform, console,
    )
    .map_err(|(pending, error)| RebuildFailure::pending(RebuildError::Assembly(error), pending))?;
    let (quiesced_platform, blocks, mems, memory_services) = paused.parts.quiesce_devices().await;
    let verified = capture_devices(paused.parts.as_ref(), &blocks, &mems, &memory_services)
        .map_err(RebuildError::Quiesce)
        .and_then(|(restored_devices, restored_memory)| {
            for (id, saved) in &devices {
                if restored_devices.get(id) != Some(saved) {
                    return Err(RebuildError::DeviceAcknowledgement(id.clone()));
                }
            }
            for (window, saved) in &populations {
                if restored_memory.get(window) != Some(saved) {
                    return Err(RebuildError::MemoryAcknowledgement(*window));
                }
            }
            capture_platform(paused.parts.as_ref(), &quiesced_platform)
                .map_err(RebuildError::Quiesce)
        });
    if let Err(error) = verified {
        return Err(RebuildFailure::pending(error, paused.into_cleanup()));
    }
    paused.record_quiesced(quiesced_platform, blocks, mems, memory_services);
    for block in paused.parts.blocks.values() {
        if let Err(error) = block.reraise_pending() {
            return Err(RebuildFailure::pending(
                RebuildError::Block(error),
                paused.into_cleanup(),
            ));
        }
    }
    for mem in paused.parts.mems.values() {
        if let Err(error) = mem.reraise_pending() {
            return Err(RebuildFailure::pending(
                RebuildError::Mem(error),
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
            memory: BTreeMap::new(),
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

    fn hotplug_fixture(
        kind: BackingKind,
        populated: bool,
    ) -> (MachineSnapshot, MemoryStore, HostCapabilities) {
        use crate::devices::virtio::mem::{MemQuiesced, MemResources, protocol};
        use crate::space::{Gpa, Range};
        use api::machine::{MemoryHotplugConfig, SetMemoryHotplug};
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
                    size_mib: 256,
                    block_size_mib: None,
                    chunk_size_mib: None,
                })
                .into_vmm_command(),
            )
            .unwrap();
        saved.plan = MachinePlan::new(&spec, &host).unwrap().0;
        let memory =
            MemoryStore::materialize(saved.plan.memory().clone(), *boot.policy(), &host).unwrap();
        let mem = saved.plan.memory_hotplug().unwrap();
        let geometry = mem.geometry();
        let ranges = if populated {
            vec![Range::new(Gpa(geometry.region().start().0 + geometry.chunk_size() - geometry.block_size()), 2 * geometry.block_size()).unwrap()]
        } else {
            vec![]
        };
        let population = Population::restore(geometry, ranges).unwrap();
        let revision = if populated { 7 } else { 0 };
        let desc = MemResources::descriptor(geometry);
        let mut config = protocol::Config::decode(&desc.config).unwrap();
        config.plugged_size = population.plugged_size();
        config.usable_region_size = if populated {
            geometry.region().len()
        } else {
            0
        };
        let snapshot = MemSnapshot {
            state: MemQuiesced {
                requested_size: 0,
                usable_region_size: config.usable_region_size,
                memory_revision: revision,
                queue: None,
            },
            transport: MmioTransportSnapshot {
                regs: MmioRegs::new(&desc),
                config: ConfigSnapshot {
                    bytes: config.encode().to_vec(),
                    generation: u32::MAX,
                },
                desc,
                interrupt_status: 3,
            },
        };
        saved.devices = BTreeMap::from([(
            crate::spec::VIRTIO_MEM_ID.into(),
            DeviceSnapshot::Mem(snapshot),
        )]);
        saved.memory = BTreeMap::from([(
            mem.window_id(),
            MemorySnapshot {
                population,
                revision,
            },
        )]);
        (saved, memory, host)
    }

    fn mem_snapshot(saved: &mut MachineSnapshot) -> &mut MemSnapshot {
        let DeviceSnapshot::Mem(mem) = saved.devices.get_mut(crate::spec::VIRTIO_MEM_ID).unwrap()
        else {
            panic!("not memory")
        };
        mem
    }

    #[test]
    fn hotplug_preflight_requires_exact_keys_geometry_and_revision_without_losing_ram() {
        for kind in [BackingKind::AnonymousPrivate, BackingKind::SharedMemfd] {
            for populated in [false, true] {
                let (mut saved, memory, host) = hotplug_fixture(kind, populated);
                assert!(preflight(&saved, &memory, &host).is_ok());
                let geometry = saved.plan.memory_hotplug().unwrap().geometry();
                let window = geometry.window_id();
                let id = crate::spec::VIRTIO_MEM_ID;
                let device = saved.devices.remove(id).unwrap();
                assert!(
                    matches!(preflight(&saved, &memory, &host), Err(RebuildError::MissingDevice(missing)) if missing == id)
                );
                let DeviceSnapshot::Mem(mem) = &device else {
                    panic!("not memory")
                };
                saved.devices.insert(
                    id.into(),
                    DeviceSnapshot::Block(BlockSnapshot {
                        transport: mem.transport.clone(),
                        queue: None,
                    }),
                );
                assert!(
                    matches!(preflight(&saved, &memory, &host), Err(RebuildError::DeviceKind(wrong)) if wrong == id)
                );
                saved.devices.insert(id.into(), device.clone());
                saved.devices.insert("unplanned".into(), device);
                assert!(
                    matches!(preflight(&saved, &memory, &host), Err(RebuildError::ExtraDevice(extra)) if extra == "unplanned")
                );
                saved.devices.remove("unplanned");
                let range = geometry.chunk(0).unwrap().placement();
                assert!(matches!(
                    Population::restore(geometry, [range, range]),
                    Err(PopulationError::DuplicateBlock(_))
                ));
                let unaligned = crate::space::Range::new(
                    crate::space::Gpa(range.start().0 + 1),
                    geometry.block_size(),
                )
                .unwrap();
                assert!(matches!(
                    Population::restore(geometry, [unaligned]),
                    Err(PopulationError::UnalignedRange { .. })
                ));
                let outside = crate::space::Range::new(
                    geometry.region().end().unwrap(),
                    geometry.block_size(),
                )
                .unwrap();
                assert!(matches!(
                    Population::restore(geometry, [outside]),
                    Err(PopulationError::OutsideGeometry(_))
                ));
                let population = saved.memory.remove(&window).unwrap();
                assert!(
                    matches!(preflight(&saved, &memory, &host), Err(RebuildError::MissingMemory(id)) if id == window)
                );
                saved.memory.insert(window, population.clone());
                saved.memory.insert(WindowId(987), population.clone());
                assert!(matches!(
                    preflight(&saved, &memory, &host),
                    Err(RebuildError::ExtraMemory(WindowId(987)))
                ));
                saved.memory.remove(&WindowId(987));
                let other = MemGeometry::new(
                    saved.plan.memory(),
                    window,
                    geometry.block_size() * 2,
                    geometry.chunk_size(),
                    memory.policy().page_size,
                )
                .unwrap();
                saved.memory.get_mut(&window).unwrap().population = Population::new(other);
                assert!(matches!(
                    preflight(&saved, &memory, &host),
                    Err(RebuildError::MemoryState {
                        source: RestoreMemoryError::Geometry,
                        ..
                    })
                ));
                saved.memory.insert(window, population);
                saved.memory.get_mut(&window).unwrap().revision += 1;
                assert!(matches!(
                    preflight(&saved, &memory, &host),
                    Err(RebuildError::MemState {
                        source: RestoreMemError::State(
                            crate::devices::virtio::mem::MemError::RestoreState
                        ),
                        ..
                    })
                ));
                let weak = Arc::downgrade(memory.view());
                let failure =
                    RebuildFailure::ready(preflight(&saved, &memory, &host).unwrap_err(), memory);
                let RebuildMemory::Ready(memory) = failure.memory else {
                    panic!("preflight retained a VM")
                };
                assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), memory.view()));
                assert_eq!(memory.policy().kind, kind);
            }
        }
    }

    #[test]
    fn hotplug_preflight_checks_actual_backing_geometry_before_vm_effects() {
        use std::error::Error as _;

        use vm_memory::{Bytes, GuestAddress};

        use crate::align::HostPageSize;

        for kind in [BackingKind::AnonymousPrivate, BackingKind::SharedMemfd] {
            for populated in [false, true] {
                let (saved, original, host) = hotplug_fixture(kind, populated);
                assert!(preflight(&saved, &original, &host).is_ok());
                let geometry = saved.plan.memory_hotplug().unwrap().geometry();
                let window = geometry.window_id();
                saved.memory[&window].validate(geometry).unwrap();
                for page in [geometry.block_size(), geometry.block_size() * 2] {
                    // A different backing policy still maps ordinary anonymous/memfd RAM.
                    let memory = MemoryStore::materialize(
                        saved.plan.memory().clone(),
                        HostPolicy {
                            kind,
                            page_size: BackingPageSize::base(HostPageSize::new(page).unwrap()),
                        },
                        &host,
                    )
                    .unwrap();
                    memory.policy().check(memory.layout(), &host).unwrap();
                    if page == geometry.block_size() {
                        assert!(preflight(&saved, &memory, &host).is_ok());
                        continue;
                    }
                    let samples = [
                        (GuestAddress(0x2000), 0xa1_u8),
                        (GuestAddress(geometry.region().start().0), 0xb2_u8),
                    ];
                    for &(address, byte) in &samples {
                        memory.view().write_obj(byte, address).unwrap();
                    }
                    let error = preflight(&saved, &memory, &host).unwrap_err();
                    let expected = GeometryError::BelowBackingPage {
                        block: geometry.block_size(),
                        page,
                    };
                    assert!(matches!(
                        &error,
                        RebuildError::MemoryState {
                            window: id,
                            source: RestoreMemoryError::BackingGeometry(source),
                        } if *id == window && *source == expected
                    ));
                    assert_eq!(
                        error
                            .source()
                            .unwrap()
                            .source()
                            .unwrap()
                            .downcast_ref::<GeometryError>(),
                        Some(&expected)
                    );
                    let weak = Arc::downgrade(memory.view());
                    let failure = RebuildFailure::ready(error, memory);
                    let RebuildMemory::Ready(memory) = failure.memory else {
                        panic!("backing preflight retained a VM")
                    };
                    assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), memory.view()));
                    for (address, byte) in samples {
                        assert_eq!(memory.view().read_obj::<u8>(address).unwrap(), byte);
                    }
                }
            }
        }
    }

    #[test]
    fn hotplug_preflight_preserves_strict_identity_and_checks_the_full_dynamic_image() {
        let (mut saved, memory, host) = hotplug_fixture(BackingKind::AnonymousPrivate, true);
        let original = mem_snapshot(&mut saved).clone();
        for field in 0..5 {
            let mem = mem_snapshot(&mut saved);
            *mem = original.clone();
            match field {
                0 => mem.transport.desc.device_type ^= 1,
                1 => mem.transport.desc.features |= 1 << 29,
                2 => mem.transport.desc.required_features = VERSION_1,
                3 => mem.transport.desc.queue_max.push(256),
                _ => mem.transport.desc.config[48] = 1,
            }
            assert!(matches!(
                preflight(&saved, &memory, &host),
                Err(RebuildError::MemState {
                    source: RestoreMemError::Transport(
                        crate::devices::virtio::mmio::RestoreError::Descriptor
                    ),
                    ..
                })
            ));
        }
        for byte in 0..56 {
            *mem_snapshot(&mut saved) = original.clone();
            mem_snapshot(&mut saved).transport.config.bytes[byte] ^= 1;
            assert!(matches!(
                preflight(&saved, &memory, &host),
                Err(RebuildError::MemState {
                    source: RestoreMemError::State(
                        crate::devices::virtio::mem::MemError::ConfigImage(_)
                    ),
                    ..
                })
            ));
        }
        *mem_snapshot(&mut saved) = original.clone();
        mem_snapshot(&mut saved).transport.config.bytes.pop();
        assert!(preflight(&saved, &memory, &host).is_err());
        *mem_snapshot(&mut saved) = original;
        for generation in [0, 1, u32::MAX] {
            mem_snapshot(&mut saved).transport.config.generation = generation;
            assert!(preflight(&saved, &memory, &host).is_ok());
        }
        mem_snapshot(&mut saved).transport.interrupt_status = 4;
        assert!(matches!(
            preflight(&saved, &memory, &host),
            Err(RebuildError::MemState {
                source: RestoreMemError::Transport(
                    crate::devices::virtio::mmio::RestoreError::Interrupt(_)
                ),
                ..
            })
        ));
    }

    #[test]
    fn hotplug_preflight_validates_target_usable_population_and_settled_wrapped_rings() {
        use crate::devices::virtio::mem::{MemError, protocol};
        use crate::devices::virtio::mmio::QueueRegs;
        use crate::space::Gpa;
        use vm_memory::{Bytes, GuestAddress};
        let (mut saved, memory, host) = hotplug_fixture(BackingKind::SharedMemfd, true);
        let original = mem_snapshot(&mut saved).clone();
        for (requested, usable) in [
            (1, 256 << 20),
            (258 << 20, 256 << 20),
            (0, 2 << 20),
            (0, 128 << 20),
            (0, 384 << 20),
        ] {
            let mem = mem_snapshot(&mut saved);
            *mem = original.clone();
            mem.state.requested_size = requested;
            mem.state.usable_region_size = usable;
            let mut config = protocol::Config::decode(&mem.transport.config.bytes).unwrap();
            config.requested_size = requested;
            config.usable_region_size = usable;
            mem.transport.config.bytes = config.encode().to_vec();
            assert!(preflight(&saved, &memory, &host).is_err());
        }
        *mem_snapshot(&mut saved) = original;
        let mem = mem_snapshot(&mut saved);
        mem.transport.regs.status = 15;
        mem.transport.regs.driver_features = protocol::REQUIRED_FEATURES;
        mem.transport.regs.queues = vec![QueueRegs {
            size: 8,
            ready: true,
            desc: Gpa(0x10000),
            avail: Gpa(0x11000),
            used: Gpa(0x12000),
        }];
        mem.state.queue = Some(virtio_queue::QueueState {
            max_size: protocol::QUEUE_MAX,
            size: 8,
            ready: true,
            event_idx_enabled: false,
            next_avail: u16::MAX,
            next_used: u16::MAX,
            desc_table: 0x10000,
            avail_ring: 0x11000,
            used_ring: 0x12000,
        });
        memory
            .view()
            .write_obj(u16::MAX, GuestAddress(0x12002))
            .unwrap();
        memory
            .view()
            .write_obj(0_u16, GuestAddress(0x11002))
            .unwrap();
        assert!(preflight(&saved, &memory, &host).is_ok());
        let active = mem_snapshot(&mut saved).clone();
        for field in 0..12 {
            let mem = mem_snapshot(&mut saved);
            *mem = active.clone();
            match field {
                0 => mem.transport.regs.driver_features = VERSION_1,
                1 => mem.transport.regs.driver_features |= 1 << 29,
                2 => mem.transport.regs.queues.push(Default::default()),
                3 => mem.state.queue.as_mut().unwrap().event_idx_enabled = true,
                4 => mem.state.queue.as_mut().unwrap().next_avail = 0,
                5 => mem.state.queue.as_mut().unwrap().ready = false,
                6 => mem.state.queue.as_mut().unwrap().max_size = 16,
                7 => mem.state.queue.as_mut().unwrap().size = 16,
                8 => mem.state.queue.as_mut().unwrap().desc_table += 16,
                9 => mem.transport.regs.queues[0].desc = Gpa(u64::MAX - 15),
                10 => mem.transport.regs.status = 7,
                _ => mem.transport.regs.queues[0].ready = false,
            }
            assert!(preflight(&saved, &memory, &host).is_err());
        }
        *mem_snapshot(&mut saved) = active;
        memory
            .view()
            .write_obj(0_u16, GuestAddress(0x12002))
            .unwrap();
        assert!(matches!(
            preflight(&saved, &memory, &host),
            Err(RebuildError::MemState {
                source: RestoreMemError::Queue(MemError::QueuePosition),
                ..
            })
        ));
        memory
            .view()
            .write_obj(u16::MAX, GuestAddress(0x12002))
            .unwrap();
        memory
            .view()
            .write_obj(8_u16, GuestAddress(0x11002))
            .unwrap();
        assert!(matches!(
            preflight(&saved, &memory, &host),
            Err(RebuildError::MemState {
                source: RestoreMemError::Queue(MemError::AvailableIndex),
                ..
            })
        ));
    }

    #[tokio::test(flavor = "local")]
    async fn hotplug_roundtrip_keeps_ram_population_generation_and_pending_state_parked() {
        use crate::devices::virtio::mem::{MemResources, protocol};
        use crate::devices::virtio::mmio::QueueRegs;
        use crate::machine::build::build_with;
        use crate::machine::platform::Captured;
        use crate::space::{Gpa, Range};
        use crate::spec::VIRTIO_MEM_ID;
        use crate::test_guest::{self, TempFile};
        use api::machine::{ConsoleConfig, MemoryHotplugConfig, SetConsole, SetMemoryHotplug};
        use std::path::Path;
        use virtio_queue::desc::split::Descriptor;
        use vm_memory::{Bytes, GuestAddress};
        const TIMEOUT: Duration = Duration::from_secs(5);
        let Some(ctx) = Path::new("/dev/kvm")
            .exists()
            .then(|| KvmContext::from_dev_path("/dev/kvm").unwrap())
        else {
            return;
        };
        let kernel = TempFile::new(&test_guest::image(&[0xeb, 0xfe], true));
        let spec = MachineSpec::default()
            .apply(
                SetKernel(KernelConfig {
                    path: kernel.path().into(),
                    cmdline: None,
                    initrd: None,
                })
                .into_vmm_command(),
            )
            .unwrap()
            .apply(SetConsole(ConsoleConfig::None).into_vmm_command())
            .unwrap()
            .apply(
                SetMemoryHotplug(MemoryHotplugConfig {
                    size_mib: 256,
                    block_size_mib: None,
                    chunk_size_mib: None,
                })
                .into_vmm_command(),
            )
            .unwrap();
        for kind in [BackingKind::AnonymousPrivate, BackingKind::SharedMemfd] {
            for populated in [false, true] {
                let (plan, mut policy) = MachinePlan::new(&spec, ctx.capabilities()).unwrap();
                policy.kind = kind;
                let geometry = plan.memory_hotplug().unwrap().geometry();
                let window = geometry.window_id();
                let (paused, _events) = build_with(
                    plan,
                    policy,
                    &ctx,
                    ConsoleBackend::Captured(Captured::default()),
                )
                .unwrap();
                let mut snapshot = paused.capture().await.unwrap();
                let memory = paused.teardown().await.unwrap();
                let weak = Arc::downgrade(memory.view());
                let first = Gpa(
                    geometry.region().start().0 + geometry.chunk_size() - geometry.block_size()
                );
                let second = Gpa(first.0 + geometry.block_size());
                if populated {
                    snapshot.memory.insert(
                        window,
                        MemorySnapshot {
                            population: Population::restore(
                                geometry,
                                [Range::new(first, 2 * geometry.block_size()).unwrap()],
                            )
                            .unwrap(),
                            revision: 7,
                        },
                    );
                    let mem = mem_snapshot(&mut snapshot);
                    let desc = MemResources::descriptor(geometry);
                    let mut config = protocol::Config::decode(&desc.config).unwrap();
                    config.usable_region_size = geometry.region().len();
                    config.plugged_size = 2 * geometry.block_size();
                    mem.state.usable_region_size = config.usable_region_size;
                    mem.state.memory_revision = 7;
                    mem.transport.config = ConfigSnapshot {
                        bytes: config.encode().to_vec(),
                        generation: u32::MAX,
                    };
                    mem.transport.interrupt_status = 3;
                    mem.transport.regs.status = 15;
                    mem.transport.regs.driver_features = protocol::REQUIRED_FEATURES;
                    mem.transport.regs.queues = vec![QueueRegs {
                        size: 8,
                        ready: true,
                        desc: Gpa(0x10000),
                        avail: Gpa(0x11000),
                        used: Gpa(0x12000),
                    }];
                    mem.state.queue = Some(virtio_queue::QueueState {
                        max_size: protocol::QUEUE_MAX,
                        size: 8,
                        ready: true,
                        event_idx_enabled: false,
                        next_avail: u16::MAX,
                        next_used: u16::MAX,
                        desc_table: 0x10000,
                        avail_ring: 0x11000,
                        used_ring: 0x12000,
                    });
                    memory
                        .view()
                        .write_obj(0xa1_u8, GuestAddress(first.0))
                        .unwrap();
                    memory
                        .view()
                        .write_obj(0xb2_u8, GuestAddress(second.0))
                        .unwrap();
                    memory
                        .view()
                        .write_slice(
                            &protocol::Request::State {
                                addr: first,
                                blocks: 2,
                            }
                            .encode(),
                            GuestAddress(0x13000),
                        )
                        .unwrap();
                    memory
                        .view()
                        .write_slice(&[0xff; protocol::RESPONSE_SIZE], GuestAddress(0x14000))
                        .unwrap();
                    memory
                        .view()
                        .write_obj(
                            Descriptor::new(0x13000, protocol::REQUEST_SIZE as u32, 1, 1),
                            GuestAddress(0x10000),
                        )
                        .unwrap();
                    memory
                        .view()
                        .write_obj(
                            Descriptor::new(0x14000, protocol::RESPONSE_SIZE as u32, 2, 0),
                            GuestAddress(0x10010),
                        )
                        .unwrap();
                    memory
                        .view()
                        .write_obj(0_u16, GuestAddress(0x11012))
                        .unwrap();
                    memory
                        .view()
                        .write_obj(0_u16, GuestAddress(0x11002))
                        .unwrap();
                    memory
                        .view()
                        .write_obj(u16::MAX, GuestAddress(0x12002))
                        .unwrap();
                }
                let devices = snapshot.devices.clone();
                let populations = snapshot.memory.clone();
                let (paused, _events) = rebuild_with(
                    snapshot,
                    memory,
                    &ctx,
                    ConsoleBackend::Captured(Captured::default()),
                )
                .await
                .unwrap();
                assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), paused.memory()));
                let captured = paused.capture().await.unwrap();
                assert_eq!(captured.devices, devices);
                assert_eq!(captured.memory, populations);
                if populated {
                    assert_eq!(
                        paused
                            .memory()
                            .read_obj::<u8>(GuestAddress(first.0))
                            .unwrap(),
                        0xa1
                    );
                    assert_eq!(
                        paused
                            .memory()
                            .read_obj::<u8>(GuestAddress(second.0))
                            .unwrap(),
                        0xb2
                    );
                    assert_eq!(
                        paused
                            .memory()
                            .read_obj::<u16>(GuestAddress(0x12002))
                            .unwrap(),
                        u16::MAX
                    );
                    let mut response = [0; protocol::RESPONSE_SIZE];
                    paused
                        .memory()
                        .read_slice(&mut response, GuestAddress(0x14000))
                        .unwrap();
                    assert_eq!(response, [0xff; protocol::RESPONSE_SIZE]);
                    let running = paused.resume().await.map_err(|(_, error)| error).unwrap();
                    tokio::time::timeout(TIMEOUT, async {
                        while running
                            .memory()
                            .read_obj::<u16>(GuestAddress(0x12002))
                            .unwrap()
                            != 0
                        {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                    let paused = running.pause().await.map_err(|(_, error)| error).unwrap();
                    paused
                        .memory()
                        .read_slice(&mut response, GuestAddress(0x14000))
                        .unwrap();
                    assert_eq!(
                        protocol::Response::decode(&response, protocol::RequestType::State)
                            .unwrap(),
                        protocol::Response::State(protocol::BlockState::Plugged)
                    );
                    let captured = paused.capture().await.unwrap();
                    assert_eq!(captured.memory, populations);
                    let DeviceSnapshot::Mem(mem) = &captured.devices[VIRTIO_MEM_ID] else {
                        panic!("not memory")
                    };
                    assert_eq!(mem.transport.config.generation, u32::MAX);
                    assert_eq!(mem.state.queue.unwrap().next_used, 0);
                    let memory = paused.teardown().await.unwrap();
                    assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), memory.view()));
                } else {
                    let memory = paused.teardown().await.unwrap();
                    assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), memory.view()));
                }
                assert!(weak.upgrade().is_none());
            }
        }
    }

    #[tokio::test(flavor = "local")]
    async fn memory_capture_requires_fresh_worker_resampler_and_live_quiesced_service_proofs() {
        use crate::machine::build::build_with;
        use crate::machine::platform::Captured;
        use crate::spec::VIRTIO_MEM_ID;
        use crate::test_guest::{self, TempFile};
        use api::machine::{ConsoleConfig, MemoryHotplugConfig, SetConsole, SetMemoryHotplug};
        use std::path::Path;
        const TIMEOUT: Duration = Duration::from_secs(5);
        let Some(ctx) = Path::new("/dev/kvm")
            .exists()
            .then(|| KvmContext::from_dev_path("/dev/kvm").unwrap())
        else {
            return;
        };
        let kernel = TempFile::new(&test_guest::image(&[0xeb, 0xfe], true));
        let spec = MachineSpec::default()
            .apply(
                SetKernel(KernelConfig {
                    path: kernel.path().into(),
                    cmdline: None,
                    initrd: None,
                })
                .into_vmm_command(),
            )
            .unwrap()
            .apply(SetConsole(ConsoleConfig::None).into_vmm_command())
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
        let (plan, policy) = MachinePlan::new(&spec, ctx.capabilities()).unwrap();
        let window = plan.memory_hotplug().unwrap().window_id();
        let (paused, _events) = build_with(
            plan,
            policy,
            &ctx,
            ConsoleBackend::Captured(Captured::default()),
        )
        .unwrap();
        let (_, blocks, mems, services) = paused.parts.quiesce_devices().await;
        assert!(capture_devices(&paused.parts, &blocks, &mems, &services).is_ok());
        let mut bad_mems = mems.clone();
        bad_mems.get_mut(VIRTIO_MEM_ID).unwrap().worker = Response::TimedOut;
        assert!(matches!(
            capture_devices(&paused.parts, &blocks, &bad_mems, &services),
            Err(CaptureError::Mem {
                source: CaptureMemError::WorkerTimedOut,
                ..
            })
        ));
        bad_mems = mems.clone();
        bad_mems.get_mut(VIRTIO_MEM_ID).unwrap().resample = Response::Exited;
        assert!(matches!(
            capture_devices(&paused.parts, &blocks, &bad_mems, &services),
            Err(CaptureError::Mem {
                source: CaptureMemError::ResampleExited,
                ..
            })
        ));
        bad_mems = mems.clone();
        let Response::Replied(state) = &mut bad_mems.get_mut(VIRTIO_MEM_ID).unwrap().worker else {
            panic!("not replied")
        };
        state.memory_revision += 1;
        assert!(matches!(
            capture_devices(&paused.parts, &blocks, &bad_mems, &services),
            Err(CaptureError::Mem {
                source: CaptureMemError::State(_),
                ..
            })
        ));
        let mut bad_services = services.clone();
        bad_services.insert(window, Response::TimedOut);
        assert!(matches!(
            capture_devices(&paused.parts, &blocks, &mems, &bad_services),
            Err(CaptureError::Memory {
                source: CaptureMemoryError::ServiceTimedOut,
                ..
            })
        ));
        bad_services = services.clone();
        let Response::Replied(state) = bad_services.get_mut(&window).unwrap() else {
            panic!("not replied")
        };
        state.revision += 1;
        assert!(matches!(
            capture_devices(&paused.parts, &blocks, &mems, &bad_services),
            Err(CaptureError::Memory {
                source: CaptureMemoryError::Changed,
                ..
            })
        ));
        assert_eq!(
            paused.parts.memory_services[&window].resume(TIMEOUT).await,
            Response::Replied(())
        );
        assert!(matches!(
            capture_devices(&paused.parts, &blocks, &mems, &services),
            Err(CaptureError::Memory {
                source: CaptureMemoryError::NotQuiesced,
                ..
            })
        ));
        assert!(matches!(
            paused.parts.memory_services[&window].quiesce(TIMEOUT).await,
            Response::Replied(_)
        ));
        paused.parts.mems[VIRTIO_MEM_ID].resample.stop();
        paused.parts.mems[VIRTIO_MEM_ID].resample.finished().await;
        assert!(matches!(
            capture_devices(&paused.parts, &blocks, &mems, &services),
            Err(CaptureError::Mem {
                source: CaptureMemError::ResampleExited,
                ..
            })
        ));
        assert_eq!(
            paused.parts.memory_services[&window]
                .stop_wait(TIMEOUT)
                .await,
            Response::Replied(())
        );
        assert!(matches!(
            capture_devices(&paused.parts, &blocks, &mems, &services),
            Err(CaptureError::Memory {
                source: CaptureMemoryError::ServiceExited,
                ..
            })
        ));
        paused.teardown().await.unwrap();
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
