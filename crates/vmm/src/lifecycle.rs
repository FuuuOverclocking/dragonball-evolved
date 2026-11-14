//! VM lifecycle, stage L1. Design notes: `ai-sessions/lifecycle.md`.
//!
//! Two layers:
//! - Decision layer ([`rules`], [`PauseTally`], [`ExitTally`], [`StopReason`]):
//!   pure functions over plain data, one rule function per phase, so a rule can
//!   only return transitions that exist from that phase.
//! - Shell ([`Vm<S>`], [`Machine`]): owns KVM, vCPU threads, devices and guest
//!   memory. The typestate `S` decides which transitions can be called;
//!   [`Machine`] is the runtime sum the VMM loop holds.
//!
//! Types and signatures only; every body is `todo!()`.
#![allow(dead_code, unused_variables)]

use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::thread::JoinHandle;

use api::ApiResult;
use api::boot::BootConfig;
use api::disk::DiskConfig;
use api::machine::ConsoleConfig;

use crate::ProcessCtx;
use crate::memory::{GuestMemory, RamSize};

pub type VmmExitStatus = anyhow::Result<()>;

// ---------------------------------------------------------------------------
// Identities
// ---------------------------------------------------------------------------

/// Index of a vCPU within the VM. Not an APIC ID: topology maps one to the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VcpuId(pub u32);

/// Names an API request whose reply is deferred. The shell keeps the reply
/// channel; the decision layer only ever sees this id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub u64);

// ---------------------------------------------------------------------------
// Observable phase
// ---------------------------------------------------------------------------

/// Projection of [`Machine`], reported as `InstanceInfo::state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// No VM resources; configuration ops edit the draft.
    Configuring,
    /// Resources exist, vCPU threads are parked at the boot entry; the guest never ran.
    Built,
    Running,
    /// vCPUs were kicked; waiting for every one of them to park.
    Pausing,
    /// Every vCPU is parked; the guest has run before.
    Paused,
    /// vCPU threads were told to exit; waiting for all of them.
    Stopping,
    /// Resources released. The VMM loop ends after reporting this.
    Stopped,
}

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// Lifecycle-relevant view of a `VmmOp`. `Pause` and `Resume` are proposed API ops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Any of `SetCpus`, `SetMemoryMib`, `SetConsole`, `SetBoot`, `AddDisk`.
    Configure(ConfigEdit),
    Build,
    Start,
    Pause,
    Resume,
    ShutdownVmm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigEdit {
    SetCpus(u32),
    SetMemoryMib(u32),
    SetConsole(ConsoleConfig),
    SetBoot(BootConfig),
    AddDisk(DiskConfig),
}

/// What a vCPU thread tells the VMM thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcpuReport {
    /// Left `KVM_RUN` after a kick and is now parked.
    Parked,
    /// The guest or KVM asked the VM to stop.
    Stop(StopReason),
    /// The thread is returning; its vCPU fd is closed.
    Exited,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Op(RequestId, Op),
    Vcpu(VcpuId, VcpuReport),
}

// ---------------------------------------------------------------------------
// Stop reasons: a join-semilattice
// ---------------------------------------------------------------------------

/// Why the VM stopped.
///
/// Declaration order is diagnostic priority. Concurrent reports merge with
/// [`StopReason::join`] = `max`, which is commutative, associative and
/// idempotent: the final reason does not depend on report arrival order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StopReason {
    /// i8042/CF9 reset, `KVM_SYSTEM_EVENT_RESET`, or triple fault (`KVM_EXIT_SHUTDOWN`).
    GuestReset,
    /// ACPI S5, `KVM_SYSTEM_EVENT_SHUTDOWN`.
    GuestPowerOff,
    /// `ShutdownVmm`, or SIGINT/SIGTERM routed through it.
    HostRequest,
    /// `KVM_SYSTEM_EVENT_CRASH`; later also pvpanic.
    GuestCrash,
    /// The VMM cannot keep this vCPU running. Ties break on `(vcpu, fault)` order.
    VcpuFault { vcpu: VcpuId, fault: VcpuFault },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VcpuFault {
    /// `KVM_EXIT_FAIL_ENTRY` with its hardware reason.
    FailEntry(u64),
    /// `KVM_EXIT_INTERNAL_ERROR` with its suberror.
    InternalError(u32),
    /// `KVM_RUN` returned an errno other than `EINTR`/`EAGAIN`.
    RunErrno(i32),
    /// An exit the VMM does not emulate.
    UnhandledExit(u32),
    /// The thread exited without being asked to.
    ThreadGone,
}

impl StopReason {
    /// `max(self, other)`.
    pub fn join(self, other: Self) -> Self {
        todo!()
    }
}

// ---------------------------------------------------------------------------
// Convergence of vCPU reports
// ---------------------------------------------------------------------------

/// Reports gathered while `Pausing`.
///
/// `record` folds one report; folding the same multiset of reports in any
/// order gives the same tally. Reports from vCPUs no longer awaited are
/// absorbed, so duplicates are harmless.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PauseTally {
    awaiting: BTreeSet<VcpuId>,
    stop: Option<StopReason>,
}

pub enum PauseVerdict {
    Pending(PauseTally),
    /// Every awaited vCPU parked and none asked to stop.
    Parked(AllParked),
    /// Some vCPU asked to stop (or died) before the pause settled.
    Stop(StopReason),
}

impl PauseTally {
    pub fn new(vcpus: BTreeSet<VcpuId>) -> Self {
        todo!()
    }

    /// `Parked` removes `vcpu` from `awaiting`; `Stop(r)` joins `r`;
    /// `Exited` joins `VcpuFault { vcpu, ThreadGone }`.
    pub fn record(self, vcpu: VcpuId, report: VcpuReport) -> Self {
        todo!()
    }

    /// `Stop` wins over `Parked`: a stop request is never lost to a pause.
    pub fn verdict(self) -> PauseVerdict {
        todo!()
    }
}

/// Reports gathered while `Stopping`. The reason only grows (by join).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitTally {
    awaiting: BTreeSet<VcpuId>,
    reason: StopReason,
}

pub enum ExitVerdict {
    Pending(ExitTally),
    Exited(AllExited, StopReason),
}

impl ExitTally {
    pub fn new(vcpus: BTreeSet<VcpuId>, reason: StopReason) -> Self {
        todo!()
    }

    /// `Exited` removes `vcpu`; `Stop(r)` joins `r`; `Parked` is absorbed.
    pub fn record(self, vcpu: VcpuId, report: VcpuReport) -> Self {
        todo!()
    }

    pub fn verdict(self) -> ExitVerdict {
        todo!()
    }
}

/// Proof that every vCPU of a pause parked. Only [`PauseTally::verdict`] makes one.
pub struct AllParked(());

/// Proof that every vCPU thread returned. Only [`ExitTally::verdict`] makes one.
pub struct AllExited(());

// ---------------------------------------------------------------------------
// Decision layer: one rule per phase
// ---------------------------------------------------------------------------

/// Pure admission rules. Each `From*` enum lists exactly the transitions that
/// exist from that phase, so the shell's `match` has no unreachable arms.
///
/// Idempotency: an op whose target phase already holds answers `Done`
/// (e.g. `Pause` while `Paused`). Clients may retry after a timeout, and a
/// timeout does not cancel the original request (see `VmmClient::call_timeout`).
pub mod rules {
    use super::{ConfigEdit, Op};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Rejection {
        /// The op needs a built VM.
        NotBuilt,
        /// A build-time op after the guest has started.
        AlreadyStarted,
        /// The draft is frozen once built. Runtime changes are a separate op family (hotplug).
        ConfigFrozen,
        /// A transient phase is settling; retry later.
        Busy,
        /// `Start` boots once; a paused guest is continued with `Resume`.
        UseResume,
        /// The VM is stopping or stopped.
        ShuttingDown,
    }

    /// The only rule that takes the op by value: `Edit` hands the payload back,
    /// so the shell never pairs a decision with an op it has to re-match.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum FromConfiguring {
        Edit(ConfigEdit),
        /// `then_start` is set for `Start`, which builds implicitly.
        Build {
            then_start: bool,
        },
        /// Nothing to tear down: the loop ends.
        Exit,
        Reject(Rejection),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FromBuilt {
        Start,
        Done,
        Stop,
        Reject(Rejection),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FromRunning {
        Pause,
        Done,
        Stop,
        Reject(Rejection),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FromPausing {
        /// Another `Pause`: reply together with the first one.
        Join,
        Stop,
        Reject(Rejection),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FromPaused {
        Resume,
        Done,
        Stop,
        Reject(Rejection),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FromStopping {
        /// `ShutdownVmm` again: already on its way.
        Done,
        Reject(Rejection),
    }

    pub fn configuring(op: Op) -> FromConfiguring {
        todo!()
    }

    pub fn built(op: &Op) -> FromBuilt {
        todo!()
    }

    pub fn running(op: &Op) -> FromRunning {
        todo!()
    }

    pub fn pausing(op: &Op) -> FromPausing {
        todo!()
    }

    pub fn paused(op: &Op) -> FromPaused {
        todo!()
    }

    pub fn stopping(op: &Op) -> FromStopping {
        todo!()
    }
}

/// vCPU reports outside a tally: `Running` turns `Stop`/`Exited` into a stop;
/// in `Built`/`Paused` only `Exited` can arrive (a parked thread died).
/// `None` means absorb.
pub fn stray_report(vcpu: VcpuId, report: VcpuReport) -> Option<StopReason> {
    todo!()
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Pre-build configuration accumulated from config ops; anything may be missing.
#[derive(Debug, Clone, Default)]
pub struct ConfigDraft {
    pub cpus: Option<u32>,
    pub memory_mib: Option<u32>,
    pub console: Option<ConsoleConfig>,
    pub boot: Option<BootConfig>,
    pub disks: Vec<DiskConfig>,
}

/// A configuration that can be built: has a boot source, at least one vCPU,
/// non-zero RAM, unique disk ids, at most one root disk.
#[derive(Debug, Clone)]
pub struct VmSpec {
    pub vcpus: NonZeroU32,
    pub ram: RamSize,
    pub console: ConsoleConfig,
    pub boot: BootConfig,
    pub disks: Vec<DiskConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigProblem {
    MissingBoot,
    ZeroCpus,
    ZeroMemory,
    DuplicateDiskId(String),
    MultipleRootDisks,
}

impl ConfigDraft {
    /// Pure. `Set*` replaces, `AddDisk` appends; validation is deferred to `validate`.
    pub fn edit(self, edit: ConfigEdit) -> Self {
        todo!()
    }

    /// Pure. Reports every problem, not just the first.
    pub fn validate(&self) -> Result<VmSpec, Vec<ConfigProblem>> {
        todo!()
    }
}

// ---------------------------------------------------------------------------
// Shell: typestate VM
// ---------------------------------------------------------------------------

/// Typestate markers for [`Vm`].
pub mod state {
    pub struct Built;
    pub struct Running;
    pub struct Pausing;
    pub struct Paused;
    pub struct Stopping;
}

mod sealed {
    pub trait Sealed {}
}

/// States from which [`Vm::stop`] exists. `Stopping` is not one: stopping twice
/// is not expressible.
pub trait Stoppable: sealed::Sealed {}

impl sealed::Sealed for state::Built {}
impl sealed::Sealed for state::Running {}
impl sealed::Sealed for state::Pausing {}
impl sealed::Sealed for state::Paused {}
impl Stoppable for state::Built {}
impl Stoppable for state::Running {}
impl Stoppable for state::Pausing {}
impl Stoppable for state::Paused {}

/// One vCPU thread, owned by the VMM thread. The thread owns the vCPU fd.
pub struct VcpuHandle {
    thread: JoinHandle<()>,
    // Command channel (run / park / exit) and the kick: a signal plus
    // `kvm_run.immediate_exit`, so a kick racing with `KVM_RUN` entry is not lost.
}

pub struct Vcpus {
    handles: BTreeMap<VcpuId, VcpuHandle>,
}

/// Device set. Out of scope here.
pub struct Devices {}

/// Resources of a built VM.
///
/// Field order is drop order and is load-bearing: vCPU threads, then
/// devices, then the VM fd (which deletes every KVM memory slot), then guest
/// memory. No slot or device outlives the host mapping it points to.
pub struct Vm<S> {
    vcpus: Vcpus,
    devices: Devices,
    vm_fd: kvm_ioctls::VmFd,
    memory: GuestMemory,
    _state: PhantomData<S>,
}

/// Creates the VM, guest memory, devices and vCPUs, loads the kernel and
/// writes the PVH entry state, then spawns parked vCPU threads.
///
/// Every fallible step of booting lives here; on error, resources built so far
/// drop in field order and the caller still holds its draft.
pub fn build(spec: &VmSpec, ctx: &ProcessCtx) -> anyhow::Result<Vm<state::Built>> {
    todo!()
}

impl Vm<state::Built> {
    /// Unparks the vCPUs. Infallible: a vCPU that cannot run reports a fault.
    pub fn start(self) -> Vm<state::Running> {
        todo!()
    }
}

impl Vm<state::Running> {
    /// Kicks every vCPU out of `KVM_RUN` and waits (via the tally) for them to park.
    pub fn pause(self) -> (Vm<state::Pausing>, PauseTally) {
        todo!()
    }
}

impl Vm<state::Pausing> {
    /// Needs the proof that every vCPU parked.
    pub fn settle(self, proof: AllParked) -> Vm<state::Paused> {
        todo!()
    }
}

impl Vm<state::Paused> {
    pub fn resume(self) -> Vm<state::Running> {
        todo!()
    }
}

impl<S: Stoppable> Vm<S> {
    /// Tells every vCPU thread to exit, kicking the ones in `KVM_RUN`.
    pub fn stop(self, reason: StopReason) -> (Vm<state::Stopping>, ExitTally) {
        todo!()
    }
}

impl Vm<state::Stopping> {
    /// Joins the vCPU threads and drops the rest in field order.
    pub fn reap(self, proof: AllExited) {
        todo!()
    }
}

// ---------------------------------------------------------------------------
// Shell: the runtime sum
// ---------------------------------------------------------------------------

/// What the VMM loop holds. [`Machine::step`] takes it by value, so each arm
/// moves the old state into the new one and no placeholder state exists.
pub enum Machine {
    Configuring(ConfigDraft),
    Built(Vm<state::Built>),
    Running(Vm<state::Running>),
    Pausing {
        vm: Vm<state::Pausing>,
        tally: PauseTally,
        /// `Pause` requests answered when the pause settles or is overtaken by a stop.
        waiters: Vec<RequestId>,
    },
    Paused(Vm<state::Paused>),
    Stopping {
        vm: Vm<state::Stopping>,
        tally: ExitTally,
    },
    Stopped(StopReason),
}

/// Replies to send after a step. Deferred requests appear in a later step.
pub type Replies = Vec<(RequestId, ApiResult<()>)>;

impl Machine {
    pub fn phase(&self) -> Phase {
        todo!()
    }

    /// One input in, one machine out. Pseudocode in `ai-sessions/lifecycle.md`.
    pub fn step(self, input: Input, ctx: &ProcessCtx) -> (Machine, Replies) {
        todo!()
    }
}
