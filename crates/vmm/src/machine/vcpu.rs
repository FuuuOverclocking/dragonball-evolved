//! vCPU threads: one OS thread per vCPU, blocking in `KVM_RUN`.
//!
//! A thread is controlled through a command channel and a kick. Kicking sets
//! `immediate_exit` through the handle's own fd of the vCPU, then signals the
//! thread, so a vCPU in the guest leaves `KVM_RUN` at once and one about to
//! enter it returns straight away. Commands are read only when `KVM_RUN` was
//! interrupted, or while paused, never right after a PIO or MMIO exit: by the
//! time a vCPU acknowledges `Pause`, the I/O it was emulating has completed.

use std::mem::MaybeUninit;
use std::os::raw::{c_int, c_void};
use std::os::unix::thread::JoinHandleExt;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{Ordering, fence};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use std::{io, mem, ptr, thread};

use kvm_bindings::{
    KVM_MAX_CPUID_ENTRIES, KVM_MP_STATE_UNINITIALIZED, KVM_REG_GUEST_SSP, KVM_SYSTEM_EVENT_RESET,
    KVM_SYSTEM_EVENT_SHUTDOWN, Msrs, Xsave, kvm_mp_state, kvm_msr_entry, kvm_x86_reg_kvm,
};
use kvm_ioctls::{VcpuExit, VmFd};
use logger::{error_unlimited, info, warn_unlimited};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use tokio::task;
use tokio::time::{self, Instant};
use vm_memory::{Bytes, GuestAddress, GuestMemoryError};
use vmm_sys_util::signal::{SIGRTMIN, register_signal_handler};
use vmm_sys_util::{errno, fam};

use super::kvm_state::{VcpuSnapshot, xsave_fam_len};
use crate::arch::x86_64::cpuid::{self, CpuidError};
use crate::arch::x86_64::{lapic, msr, regs};
use crate::bus::{MmioBus, PioBus};
use crate::error::render_chain;
use crate::kvm_context::HostCapabilities;
use crate::machine::MachineEvent;
use crate::memory::store::MemoryView;
use crate::space::{Gpa, Pio};
use crate::vm::{KvmVcpu, KvmVcpuError};

/// What vCPUs dispatch PIO and MMIO exits to.
#[derive(Debug, Default)]
pub(super) struct Buses {
    pub(super) pio: PioBus,
    pub(super) mmio: MmioBus,
}

/// A step of configuring a vCPU through KVM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display)]
pub enum SetupStep {
    /// set the CPUID
    SetCpuid,
    /// set the boot MSRs
    SetMsrs,
    /// set the general registers
    SetRegs,
    /// set the FPU state
    SetFpu,
    /// get the special registers
    GetSregs,
    /// set the special registers
    SetSregs,
    /// set the multiprocessing state
    SetMpState,
    /// get the LAPIC
    GetLapic,
    /// set the LAPIC
    SetLapic,
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum ConfigureVcpuError {
    /// normalize the CPUID of vCPU {index}
    Cpuid {
        index: u8,
        #[source]
        source: CpuidError,
    },
    /// build the boot MSRs
    Msrs(#[source] fam::Error),
    /// KVM set {set} of the {total} boot MSRs of vCPU {index}
    PartialMsrs { index: u8, set: usize, total: usize },
    /// write the descriptor tables of vCPU {index}
    Tables {
        index: u8,
        #[source]
        source: GuestMemoryError,
    },
    /// {step} of vCPU {index}
    Kvm {
        index: u8,
        step: SetupStep,
        #[source]
        source: kvm_ioctls::Error,
    },
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum SpawnVcpuError {
    /// install the vCPU kick signal handler
    SignalHandler(#[source] errno::Error),
    /// open a kick fd for vCPU {index}
    KickFd {
        index: u8,
        #[source]
        source: KvmVcpuError,
    },
    /// spawn the thread of vCPU {index}
    Thread {
        index: u8,
        #[source]
        source: io::Error,
    },
}

/// Why a request to the vCPUs did not complete.
#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum VcpuError {
    /// vCPU {0} has exited
    Exited(u8),
    /// vCPU {0} did not answer in time
    TimedOut(u8),
    /// vCPU {0} thread panicked
    Panicked(u8),
    /// join the thread of vCPU {index}
    Join {
        index: u8,
        #[source]
        source: task::JoinError,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display)]
pub enum CaptureStep {
    /// get the multiprocessing state
    MpState,
    /// get the general registers
    Regs,
    /// get the special registers
    Sregs,
    /// get the XSAVE state
    Xsave,
    /// get the LAPIC
    Lapic,
    /// get the CPUID
    Cpuid,
    /// get the MSRs
    Msrs,
    /// get the active shadow stack pointer through KVM_GET_ONE_REG
    Ssp,
    /// get the XCRs
    Xcrs,
    /// get the debug registers
    DebugRegs,
    /// get the vCPU events
    Events,
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum CaptureVcpuError {
    /// vCPU {0} has exited or no longer has a live thread handle
    Exited(u8),
    /// vCPU {0} is not known to be paused
    NotPaused(u8),
    /// build the snapshot MSR query
    Msrs(#[from] fam::Error),
    /// VM of vCPU {0} does not report a valid KVM_CAP_XSAVE2 buffer size
    XsaveSize(u8),
    /// allocate the full XSAVE buffer of vCPU {index}
    XsaveAllocation {
        index: u8,
        #[source]
        source: fam::Error,
    },
    /// vCPU {0} has unsupported supervisor or inconsistent XSAVE state
    Xstate(u8),
    /// KVM read {read} of the {total} snapshot MSRs of vCPU {index}
    PartialMsrs {
        index: u8,
        read: usize,
        total: usize,
    },
    /// {step} of vCPU {index}
    Kvm {
        index: u8,
        step: CaptureStep,
        #[source]
        source: kvm_ioctls::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display)]
pub enum RestoreStep {
    /// restore the CPUID
    Cpuid,
    /// restore the multiprocessing state
    MpState,
    /// restore the general registers
    Regs,
    /// restore the special registers
    Sregs,
    /// restore the XSAVE state
    Xsave,
    /// restore the XCRs
    Xcrs,
    /// restore the debug registers
    DebugRegs,
    /// restore the LAPIC
    Lapic,
    /// restore the xstate control MSRs before XSAVE
    XstateMsrs,
    /// restore the remaining MSRs, including CET supervisor state
    Msrs,
    /// restore the active shadow stack pointer through KVM_SET_ONE_REG after XSAVE and S_CET
    Ssp,
    /// restore the wall-clock registration after the VM clock
    WallClock,
    /// restore the vCPU events
    Events,
    /// notify KVM that the guest clock may be out of sync
    Clock,
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum RestoreVcpuError {
    /// build the snapshot MSR set
    Msrs(#[from] fam::Error),
    /// saved XSAVE buffer of vCPU {0} does not match the VM's KVM_CAP_XSAVE2 size
    XsaveSize(u8),
    /// saved active SSP presence of vCPU {0} does not match CPUID shadow stack support
    Ssp(u8),
    /// vCPU {0} has unsupported supervisor or inconsistent XSAVE state
    Xstate(u8),
    /// KVM set {set} of the {total} snapshot MSRs of vCPU {index}
    PartialMsrs { index: u8, set: usize, total: usize },
    /// {step} of vCPU {index}
    Kvm {
        index: u8,
        step: RestoreStep,
        #[source]
        source: kvm_ioctls::Error,
    },
}

/// Why a vCPU stopped running its guest. `Copy`, so the exit path builds it
/// without allocating.
#[derive(Debug, Clone, Copy, displaydoc::Display, thiserror::Error)]
enum VcpuFault {
    /// KVM_RUN failed
    Run(#[source] kvm_ioctls::Error),
    /// KVM could not enter the guest, hardware reason {0:#x}
    FailEntry(u64),
    /// KVM reported an internal error
    Internal,
    /// unexpected KVM exit, reason {0}
    Unexpected(u32),
    /// unblock the vCPU kick signal
    Unblock(#[source] errno::Error),
    /// vCPU thread panicked
    Panicked,
}

/// Configures vCPU `index` of `count` to boot: vCPU 0 at the PVH `entry`, the
/// others waiting for INIT and SIPI. Writes vCPU 0's descriptor tables to
/// `memory`.
pub(super) fn configure(
    vcpu: &KvmVcpu,
    host: &HostCapabilities,
    count: u8,
    index: u8,
    entry: Gpa,
    memory: &MemoryView,
) -> Result<(), ConfigureVcpuError> {
    let kvm = |step| {
        move |source: kvm_ioctls::Error| ConfigureVcpuError::Kvm {
            index,
            step,
            source,
        }
    };
    let cpuid = cpuid::normalize(&host.supported_cpuid, &host.host_cpuid, count, index)
        .map_err(|source| ConfigureVcpuError::Cpuid { index, source })?;
    // KVM decides from the CPUID which MSRs exist, so it goes first.
    vcpu.set_cpuid2(&cpuid).map_err(kvm(SetupStep::SetCpuid))?;
    let entries = msr::boot_msrs();
    let msrs = Msrs::from_entries(&entries).map_err(ConfigureVcpuError::Msrs)?;
    let set = vcpu.set_msrs(&msrs).map_err(kvm(SetupStep::SetMsrs))?;
    if set != entries.len() {
        return Err(ConfigureVcpuError::PartialMsrs {
            index,
            set,
            total: entries.len(),
        });
    }
    if index == 0 {
        vcpu.set_regs(&regs::boot_regs(entry))
            .map_err(kvm(SetupStep::SetRegs))?;
        vcpu.set_fpu(&regs::boot_fpu())
            .map_err(kvm(SetupStep::SetFpu))?;
        let sregs = vcpu.get_sregs().map_err(kvm(SetupStep::GetSregs))?;
        let (sregs, tables) = regs::boot_sregs(sregs);
        for table in tables {
            memory
                .write_slice(&table.bytes, GuestAddress(table.address.0))
                .map_err(|source| ConfigureVcpuError::Tables { index, source })?;
        }
        vcpu.set_sregs(&sregs).map_err(kvm(SetupStep::SetSregs))?;
    } else {
        vcpu.set_mp_state(kvm_mp_state {
            mp_state: KVM_MP_STATE_UNINITIALIZED,
        })
        .map_err(kvm(SetupStep::SetMpState))?;
    }
    let lapic = vcpu.get_lapic().map_err(kvm(SetupStep::GetLapic))?;
    vcpu.set_lapic(&lapic::configure_lint(lapic))
        .map_err(kvm(SetupStep::SetLapic))
}

#[derive(Debug)]
enum VcpuCommand {
    Pause(oneshot::Sender<()>),
    Resume(oneshot::Sender<()>),
    Stop,
}

/// The vCPU threads of one machine, all parked until the first resume.
#[derive(Debug)]
pub(super) struct Vcpus(Vec<VcpuHandle>);

impl Vcpus {
    /// Failure retains started threads for cleanup; unstarted fds close before returning.
    pub(super) fn spawn_retained(
        vcpus: Vec<KvmVcpu>,
        buses: Arc<Buses>,
        events: tokio_mpsc::UnboundedSender<MachineEvent>,
    ) -> Result<Self, (Self, SpawnVcpuError)> {
        let mut started = Self(Vec::with_capacity(vcpus.len()));
        if let Err(error) = install_kick_handler() {
            return Err((started, error));
        }
        for (index, fd) in (0..=u8::MAX).zip(vcpus) {
            match VcpuHandle::spawn(index, fd, Arc::clone(&buses), events.clone()) {
                Ok(handle) => started.0.push(handle),
                Err(error) => return Err((started, error)),
            }
        }
        Ok(started)
    }

    pub(super) fn len(&self) -> usize {
        self.0.len()
    }

    /// Asks every vCPU to pause, and waits at most `within` for all of them.
    /// Only an acknowledgement proves the pause barrier was reached; a queued
    /// command and its kick alone do not prove that the vCPU is parked.
    pub(super) async fn pause(&mut self, within: Duration) -> Result<(), VcpuError> {
        self.request(VcpuCommand::Pause, State::Paused, within)
            .await
    }

    /// Asks every vCPU to run, and waits at most `within` for all of them.
    pub(super) async fn resume(&mut self, within: Duration) -> Result<(), VcpuError> {
        self.request(VcpuCommand::Resume, State::Running, within)
            .await
    }

    /// Timeouts retain exit/join ownership for retry; completed handles are always removed.
    pub(super) async fn stop_wait(&mut self, within: Duration) -> Result<(), VcpuError> {
        let deadline = Instant::now() + within;
        for handle in &mut self.0 {
            handle.command(VcpuCommand::Stop);
        }
        let mut first = None;
        for handle in &mut self.0 {
            if let Err(error) = handle.thread.join_wait(handle.index, deadline).await {
                warn_unlimited!("{}", render_chain(&error));
                first.get_or_insert(error);
            }
        }
        self.0.retain(|handle| !handle.thread.is_joined());
        first.map_or(Ok(()), Err)
    }

    /// Capture requires live, currently acknowledged parked threads, not a stale Pause ack.
    pub(super) fn capture_state(&self, vm: &VmFd) -> Result<Vec<VcpuSnapshot>, CaptureVcpuError> {
        for handle in &self.0 {
            handle.ensure_capturable()?;
        }
        let states = self
            .0
            .iter()
            .map(|handle| handle.capture_state(vm))
            .collect::<Result<_, _>>()?;
        // Refuse an exit that occurred while another CPU's state was read.
        for handle in &self.0 {
            handle.ensure_capturable()?;
        }
        Ok(states)
    }

    async fn request(
        &mut self,
        command: fn(oneshot::Sender<()>) -> VcpuCommand,
        state: State,
        within: Duration,
    ) -> Result<(), VcpuError> {
        let deadline = Instant::now() + within;
        let answers: Vec<_> = self
            .0
            .iter_mut()
            .map(|handle| {
                let (reply, answer) = oneshot::channel();
                handle.command(command(reply));
                (handle.index, answer)
            })
            .collect();
        wait_for_replies(answers, deadline).await?;
        for handle in &mut self.0 {
            handle.state = Some(state);
        }
        Ok(())
    }
}

/// Waits for the whole cohort under one deadline. An unanswered CPU is fatal
/// even when another CPU has already exited (possibly because of a reset).
async fn wait_for_replies(
    answers: Vec<(u8, oneshot::Receiver<()>)>,
    deadline: Instant,
) -> Result<(), VcpuError> {
    let mut failure = None;
    for (index, answer) in answers {
        let error = match time::timeout_at(deadline, answer).await {
            Ok(Ok(())) => continue,
            // The thread has ended, dropping the reply slot with its commands.
            Ok(Err(_)) => VcpuError::Exited(index),
            Err(_) => VcpuError::TimedOut(index),
        };
        if failure.is_none()
            || matches!(
                (&failure, &error),
                (Some(VcpuError::Exited(_)), VcpuError::TimedOut(_))
            )
        {
            failure = Some(error);
        }
    }
    failure.map_or(Ok(()), Err)
}

/// vmm-main's side of one vCPU thread. Dropping it asks the thread to stop,
/// without waiting for it.
#[derive(Debug)]
struct VcpuHandle {
    index: u8,
    commands: mpsc::Sender<VcpuCommand>,
    // The handle's own fd of the vCPU, to set `immediate_exit` with.
    kick: KvmVcpu,
    thread: VcpuThread,
    // Invalidated before queuing any command; restored only by a cohort ack.
    state: Option<State>,
}

/// Timeout retains both exit notification and join ownership for retry.
#[derive(Debug)]
struct VcpuThread {
    exited: Option<oneshot::Receiver<()>>,
    owner: ThreadOwner,
}

#[derive(Debug)]
enum ThreadOwner {
    // Keeps pthread_t valid for kicks until the thread is handed to join.
    Running(thread::JoinHandle<()>),
    Joining(task::JoinHandle<thread::Result<()>>),
    Joined,
}

impl VcpuHandle {
    fn spawn(
        index: u8,
        fd: KvmVcpu,
        buses: Arc<Buses>,
        events: tokio_mpsc::UnboundedSender<MachineEvent>,
    ) -> Result<Self, SpawnVcpuError> {
        let kick = fd
            .try_clone()
            .map_err(|source| SpawnVcpuError::KickFd { index, source })?;
        let (commands, receiver) = mpsc::channel();
        let (exited_tx, exited) = oneshot::channel();
        let runner = VcpuRunner {
            index,
            fd,
            buses,
            commands: receiver,
        };
        let thread = thread::Builder::new()
            .name(format!("vcpu-{index}"))
            .spawn(move || {
                let ending = runner.run_thread();
                match ending {
                    Ending::Stopped => {}
                    Ending::Reset => {
                        let _ = events.send(MachineEvent::GuestReset);
                    }
                    Ending::Failed(fault) => {
                        let error = render_chain(&fault);
                        error_unlimited!("vCPU {index} failed: {error}");
                        let _ = events.send(MachineEvent::VcpuFailed { index, error });
                    }
                }
                let _ = exited_tx.send(());
            })
            .map_err(|source| SpawnVcpuError::Thread { index, source })?;
        Ok(Self {
            index,
            commands,
            kick,
            thread: VcpuThread {
                exited: Some(exited),
                owner: ThreadOwner::Running(thread),
            },
            state: Some(State::Paused),
        })
    }

    /// Queues `command`, then kicks. A thread that has ended has dropped its
    /// receiver, and the reply slot goes with the command.
    fn command(&mut self, command: VcpuCommand) {
        self.state = None;
        if self.commands.send(command).is_ok() {
            self.kick();
        }
    }

    fn kick(&mut self) {
        self.kick.set_kvm_immediate_exit(1);
        fence(Ordering::SeqCst);
        let ThreadOwner::Running(thread) = &self.thread.owner else {
            return;
        };
        // SAFETY: the thread is not joined while its handle is held, so its pthread_t is valid.
        let errno = unsafe { libc::pthread_kill(thread.as_pthread_t(), SIGRTMIN()) };
        if errno != 0 && errno != libc::ESRCH {
            error_unlimited!(
                "kick vCPU {}: {}",
                self.index,
                io::Error::from_raw_os_error(errno)
            );
        }
    }

    fn ensure_capturable(&self) -> Result<(), CaptureVcpuError> {
        if !self.thread.is_alive() {
            return Err(CaptureVcpuError::Exited(self.index));
        }
        if self.state != Some(State::Paused) {
            return Err(CaptureVcpuError::NotPaused(self.index));
        }
        Ok(())
    }

    fn capture_state(&self, vm: &VmFd) -> Result<VcpuSnapshot, CaptureVcpuError> {
        let index = self.index;
        let fd = &self.kick;
        let kvm = |step| {
            move |source| CaptureVcpuError::Kvm {
                index,
                step,
                source,
            }
        };
        // KVM_GET_MP_STATE may accept APIC events and change LAPIC state.
        let mp_state = fd.get_mp_state().map_err(kvm(CaptureStep::MpState))?;
        let regs = fd.get_regs().map_err(kvm(CaptureStep::Regs))?;
        let sregs = fd.get_sregs().map_err(kvm(CaptureStep::Sregs))?;
        let lapic = fd.get_lapic().map_err(kvm(CaptureStep::Lapic))?;
        let cpuid = fd
            .get_cpuid2(KVM_MAX_CPUID_ENTRIES)
            .map_err(kvm(CaptureStep::Cpuid))?;
        let entries: Vec<_> = msr::snapshot_msr_indices(&cpuid)
            .into_iter()
            .map(|index| kvm_msr_entry {
                index,
                ..Default::default()
            })
            .collect();
        let mut query = Msrs::from_entries(&entries)?;
        let read = fd.get_msrs(&mut query).map_err(kvm(CaptureStep::Msrs))?;
        // KVM stops at the first unsupported MSR; a partial required-state read is not a snapshot.
        if read != entries.len() {
            return Err(CaptureVcpuError::PartialMsrs {
                index,
                read,
                total: entries.len(),
            });
        }
        let msrs = query.as_slice().to_vec();
        let xcrs = fd.get_xcrs().map_err(kvm(CaptureStep::Xcrs))?;
        let fam_len = xsave_fam_len(vm).ok_or(CaptureVcpuError::XsaveSize(index))?;
        let mut xsave = Xsave::new(fam_len)
            .map_err(|source| CaptureVcpuError::XsaveAllocation { index, source })?;
        // SAFETY: full VM XSAVE2 FAM, parked vCPU, and unchanged dynamic user permissions through GET.
        unsafe { fd.get_xsave2(&mut xsave) }.map_err(kvm(CaptureStep::Xsave))?;
        let ssp = if msr::has_shadow_stack(&cpuid) {
            let mut data = [0; mem::size_of::<u64>()];
            fd.get_one_reg(kvm_x86_reg_kvm(KVM_REG_GUEST_SSP), &mut data)
                .map_err(kvm(CaptureStep::Ssp))?;
            Some(u64::from_ne_bytes(data))
        } else {
            None
        };
        let debug_regs = fd.get_debug_regs().map_err(kvm(CaptureStep::DebugRegs))?;
        // Other state reads can change pending events, so these must be last.
        let vcpu_events = fd.get_vcpu_events().map_err(kvm(CaptureStep::Events))?;
        let snapshot = VcpuSnapshot {
            index,
            mp_state,
            regs,
            sregs,
            xsave,
            lapic,
            cpuid,
            msrs,
            ssp,
            xcrs,
            debug_regs,
            vcpu_events,
        };
        if !snapshot.xstate_supported() {
            return Err(CaptureVcpuError::Xstate(index));
        }
        Ok(snapshot)
    }
}

impl VcpuThread {
    fn is_alive(&self) -> bool {
        matches!(&self.owner, ThreadOwner::Running(thread) if !thread.is_finished())
            && self
                .exited
                .as_ref()
                .is_some_and(|exited| !exited.is_terminated() && exited.is_empty())
    }

    fn is_joined(&self) -> bool {
        matches!(self.owner, ThreadOwner::Joined)
    }

    async fn join_wait(&mut self, index: u8, deadline: Instant) -> Result<(), VcpuError> {
        if let Some(exited) = &mut self.exited {
            // Join even after a closed exit sender to distinguish unwinding from clean exit.
            let _ = time::timeout_at(deadline, exited)
                .await
                .map_err(|_| VcpuError::TimedOut(index))?;
            self.exited = None;
        }
        if matches!(self.owner, ThreadOwner::Running(_)) {
            let ThreadOwner::Running(thread) = mem::replace(&mut self.owner, ThreadOwner::Joined)
            else {
                unreachable!("the thread is still owned here");
            };
            self.owner = ThreadOwner::Joining(task::spawn_blocking(move || thread.join()));
        }
        let result = match &mut self.owner {
            ThreadOwner::Joining(join) => time::timeout_at(deadline, join)
                .await
                .map_err(|_| VcpuError::TimedOut(index))?,
            ThreadOwner::Joined => return Ok(()),
            ThreadOwner::Running(_) => unreachable!("the blocking pool owns the join"),
        };
        // Completed joins, including panics, must release kick fds and never be polled twice.
        self.owner = ThreadOwner::Joined;
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(VcpuError::Panicked(index)),
            Err(source) => Err(VcpuError::Join { index, source }),
        }
    }
}

/// XCR0/XSS/XFD precede XSAVE; LAPIC precedes TSC deadline; events follow MSRs.
pub(super) fn restore_state(
    fd: &KvmVcpu,
    vm: &VmFd,
    snapshot: &VcpuSnapshot,
) -> Result<(), RestoreVcpuError> {
    let index = snapshot.index;
    if !snapshot.ssp_matches_cpuid() {
        return Err(RestoreVcpuError::Ssp(index));
    }
    if !snapshot.xstate_supported() {
        return Err(RestoreVcpuError::Xstate(index));
    }
    let kvm = |step| {
        move |source| RestoreVcpuError::Kvm {
            index,
            step,
            source,
        }
    };
    fd.set_cpuid2(&snapshot.cpuid)
        .map_err(kvm(RestoreStep::Cpuid))?;
    fd.set_mp_state(snapshot.mp_state)
        .map_err(kvm(RestoreStep::MpState))?;
    fd.set_regs(&snapshot.regs)
        .map_err(kvm(RestoreStep::Regs))?;
    fd.set_sregs(&snapshot.sregs)
        .map_err(kvm(RestoreStep::Sregs))?;
    fd.set_xcrs(&snapshot.xcrs)
        .map_err(kvm(RestoreStep::Xcrs))?;
    let controls: Vec<_> = snapshot
        .msrs
        .iter()
        .copied()
        .filter(|entry| msr::is_xstate_control(entry.index))
        .collect();
    restore_msrs(fd, index, &controls, RestoreStep::XstateMsrs)?;
    // CPUID may allocate dynamic xstate, so validate the full target size after installing it.
    if xsave_fam_len(vm) != Some(snapshot.xsave.as_slice().len()) {
        return Err(RestoreVcpuError::XsaveSize(index));
    }
    // SAFETY: same-host full VM XSAVE2 FAM and unchanged dynamic permissions through SET; XFD is restored.
    unsafe { fd.set_xsave2(&snapshot.xsave) }.map_err(kvm(RestoreStep::Xsave))?;
    fd.set_debug_regs(&snapshot.debug_regs)
        .map_err(kvm(RestoreStep::DebugRegs))?;
    fd.set_lapic(&snapshot.lapic)
        .map_err(kvm(RestoreStep::Lapic))?;
    // Supervisor CET state is outside the XSAVE UABI and must be installed after SET_XSAVE.
    let remaining: Vec<_> = snapshot
        .msrs
        .iter()
        .copied()
        .filter(|entry| !msr::is_xstate_control(entry.index))
        .collect();
    restore_msrs(fd, index, &remaining, RestoreStep::Msrs)?;
    if let Some(ssp) = snapshot.ssp {
        fd.set_one_reg(kvm_x86_reg_kvm(KVM_REG_GUEST_SSP), &ssp.to_ne_bytes())
            .map_err(kvm(RestoreStep::Ssp))?;
    }
    fd.set_vcpu_events(&snapshot.vcpu_events)
        .map_err(kvm(RestoreStep::Events))?;
    // KVM_CLOCK_CTRL needs an active pv_time cache; disabled registrations legitimately have none.
    if msr::pvclock_enabled(&snapshot.msrs) {
        fd.kvmclock_ctrl().map_err(kvm(RestoreStep::Clock))?;
    }
    Ok(())
}

/// Replay wall-clock aliases in captured order after SET_CLOCK, which determines the epoch written to RAM.
pub(super) fn restore_wall_clock(
    fd: &KvmVcpu,
    snapshot: &VcpuSnapshot,
) -> Result<(), RestoreVcpuError> {
    let entries: Vec<_> = msr::wall_clock_msrs(&snapshot.msrs).collect();
    restore_msrs(fd, snapshot.index, &entries, RestoreStep::WallClock)
}

fn restore_msrs(
    fd: &KvmVcpu,
    index: u8,
    entries: &[kvm_msr_entry],
    step: RestoreStep,
) -> Result<(), RestoreVcpuError> {
    if entries.is_empty() {
        return Ok(());
    }
    let msrs = Msrs::from_entries(entries)?;
    let set = fd.set_msrs(&msrs).map_err(|source| RestoreVcpuError::Kvm {
        index,
        step,
        source,
    })?;
    if set != entries.len() {
        return Err(RestoreVcpuError::PartialMsrs {
            index,
            set,
            total: entries.len(),
        });
    }
    Ok(())
}

impl Drop for VcpuHandle {
    fn drop(&mut self) {
        if matches!(self.thread.owner, ThreadOwner::Running(_)) {
            self.command(VcpuCommand::Stop);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Paused,
    Running,
    Stopped,
}

/// The result of one `KVM_RUN`.
enum Exit {
    /// An I/O exit, completed on this thread; re-enter without reading commands.
    Handled,
    /// Kicked or signalled: read commands.
    Interrupted,
    /// The guest reset or shut down.
    Reset,
    Failed(VcpuFault),
    /// An exit with no handling; its reason is read once the exit is released.
    Unexpected,
}

/// How a vCPU thread ended.
enum Ending {
    Stopped,
    Reset,
    Failed(VcpuFault),
}

/// What a vCPU thread owns.
struct VcpuRunner {
    index: u8,
    fd: KvmVcpu,
    buses: Arc<Buses>,
    commands: mpsc::Receiver<VcpuCommand>,
}

impl VcpuRunner {
    /// Runs until stopped, then closes the vCPU fd before reporting.
    fn run_thread(mut self) -> Ending {
        let ending = match unblock_kick() {
            Ok(()) => panic::catch_unwind(AssertUnwindSafe(|| self.run()))
                .unwrap_or(Ending::Failed(VcpuFault::Panicked)),
            Err(error) => Ending::Failed(VcpuFault::Unblock(error)),
        };
        drop(self);
        ending
    }

    fn run(&mut self) -> Ending {
        let mut state = State::Paused;
        loop {
            state = match state {
                State::Paused => match self.commands.recv() {
                    Ok(command) => self.apply(State::Paused, Some(command)),
                    Err(_) => State::Stopped,
                },
                State::Running => match self.run_once() {
                    Exit::Handled => State::Running,
                    Exit::Interrupted => self.apply(State::Running, None),
                    Exit::Reset => return Ending::Reset,
                    Exit::Failed(fault) => return Ending::Failed(fault),
                    Exit::Unexpected => {
                        let reason = self.fd.get_kvm_run().exit_reason;
                        return Ending::Failed(VcpuFault::Unexpected(reason));
                    }
                },
                State::Stopped => return Ending::Stopped,
            };
        }
    }

    /// Clears the kick, then applies `first` and every queued command in order.
    fn apply(&mut self, mut state: State, first: Option<VcpuCommand>) -> State {
        // Commands are queued before their kick, so any command whose kick
        // this clears is read below.
        self.fd.set_kvm_immediate_exit(0);
        fence(Ordering::SeqCst);
        let mut next = first;
        loop {
            let command = match next.take() {
                Some(command) => command,
                None => match self.commands.try_recv() {
                    Ok(command) => command,
                    Err(TryRecvError::Empty) => return state,
                    Err(TryRecvError::Disconnected) => return State::Stopped,
                },
            };
            state = match command {
                VcpuCommand::Pause(ack) => {
                    let _ = ack.send(());
                    State::Paused
                }
                VcpuCommand::Resume(ack) => {
                    let _ = ack.send(());
                    State::Running
                }
                VcpuCommand::Stop => return State::Stopped,
            };
        }
    }

    fn run_once(&mut self) -> Exit {
        let index = self.index;
        match self.fd.run() {
            Ok(VcpuExit::IoIn(port, data)) => {
                data.fill(0);
                if !self.buses.pio.read(Pio(port), data) {
                    info!("vCPU {index}: no device reads port {port:#x}");
                }
                Exit::Handled
            }
            Ok(VcpuExit::IoOut(port, data)) => {
                if !self.buses.pio.write(Pio(port), data) {
                    info!("vCPU {index}: no device writes port {port:#x}");
                }
                Exit::Handled
            }
            Ok(VcpuExit::MmioRead(address, data)) => {
                data.fill(0);
                if !self.buses.mmio.read(Gpa(address), data) {
                    info!("vCPU {index}: no device reads address {address:#x}");
                }
                Exit::Handled
            }
            Ok(VcpuExit::MmioWrite(address, data)) => {
                if !self.buses.mmio.write(Gpa(address), data) {
                    info!("vCPU {index}: no device writes address {address:#x}");
                }
                Exit::Handled
            }
            Ok(
                VcpuExit::Shutdown
                | VcpuExit::SystemEvent(KVM_SYSTEM_EVENT_RESET | KVM_SYSTEM_EVENT_SHUTDOWN, _),
            ) => Exit::Reset,
            Ok(VcpuExit::Intr) => Exit::Interrupted,
            Ok(VcpuExit::FailEntry(reason, _)) => Exit::Failed(VcpuFault::FailEntry(reason)),
            Ok(VcpuExit::InternalError) => Exit::Failed(VcpuFault::Internal),
            // With the irqchip in the kernel, `Hlt` among them.
            Ok(_) => Exit::Unexpected,
            Err(error) if error.errno() == libc::EINTR => Exit::Interrupted,
            Err(error) if error.errno() == libc::EAGAIN => Exit::Handled,
            Err(error) => Exit::Failed(VcpuFault::Run(error)),
        }
    }
}

static KICK_HANDLER: OnceLock<Result<(), errno::Error>> = OnceLock::new();

extern "C" fn on_kick(_: c_int, _: *mut libc::siginfo_t, _: *mut c_void) {
    fence(Ordering::Acquire);
}

/// Installs the kick handler, once per process, before any vCPU thread runs.
fn install_kick_handler() -> Result<(), SpawnVcpuError> {
    (*KICK_HANDLER.get_or_init(|| register_signal_handler(SIGRTMIN(), on_kick)))
        .map_err(SpawnVcpuError::SignalHandler)
}

/// Unblocks the kick signal, in case this thread inherited a mask blocking it.
fn unblock_kick() -> Result<(), errno::Error> {
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: sigemptyset initializes the set before sigaddset and pthread_sigmask read it.
    let error = unsafe {
        libc::sigemptyset(set.as_mut_ptr());
        libc::sigaddset(set.as_mut_ptr(), SIGRTMIN());
        libc::pthread_sigmask(libc::SIG_UNBLOCK, set.as_ptr(), ptr::null_mut())
    };
    if error == 0 {
        Ok(())
    } else {
        Err(errno::Error::new(error))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::Path;
    use std::sync::Mutex;

    use super::*;
    use crate::align::BackingPageSize;
    use crate::arch::x86_64::layout::{COM1_GSI, COM1_PORTS, I8042_PORTS, PVH_INFO_START};
    use crate::bus::BusDevice;
    use crate::devices::legacy::i8042::I8042Device;
    use crate::devices::legacy::serial::SerialDevice;
    use crate::kvm_context::KvmContext;
    use crate::memory::layout::{
        MemoryLayout, Object, ObjectId, Window, WindowId, WindowKind, WindowOwner,
    };
    use crate::memory::policy::{BackingKind, HostPolicy};
    use crate::memory::store::MemoryStore;
    use crate::space::{ObjectOffset, Range};
    use crate::vm::{Binding, KvmVm, create_vm};

    const TIMEOUT: Duration = Duration::from_secs(2);
    const ENTRY: Gpa = Gpa(0x10_0000);

    /// The host's KVM, unless the host has none.
    fn kvm() -> Option<KvmContext> {
        Path::new("/dev/kvm")
            .exists()
            .then(|| KvmContext::from_dev_path("/dev/kvm").unwrap())
    }

    fn store(context: &KvmContext, layout: MemoryLayout, kind: BackingKind) -> MemoryStore {
        let host = context.capabilities();
        let policy = HostPolicy {
            kind,
            page_size: BackingPageSize::base(host.page_size),
        };
        MemoryStore::materialize(layout, policy, host).unwrap()
    }

    /// A VM with 16 MiB of boot RAM bound.
    fn boot_vm(context: &KvmContext) -> (Arc<KvmVm>, Vec<Binding>) {
        let memory = store(
            context,
            MemoryLayout::boot(16 << 20).unwrap(),
            BackingKind::AnonymousPrivate,
        );
        let (vm, bindings, _) = create_vm(context, memory).unwrap();
        (vm, bindings)
    }

    /// vCPU 0 of a one-vCPU machine, set up to run `code` from `ENTRY`.
    fn bsp(context: &KvmContext, vm: &Arc<KvmVm>, code: &[u8]) -> KvmVcpu {
        let memory = vm.memory().view();
        memory.write_slice(code, GuestAddress(ENTRY.0)).unwrap();
        let vcpu = vm.create_vcpu(0).unwrap();
        configure(&vcpu, context.capabilities(), 1, 0, ENTRY, memory).unwrap();
        vcpu
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    const MMIO_READ_VALUE: u32 = 0x4433_2211;

    /// Answers a 4-byte read, then blocks the vCPU until released.
    struct BlockingReadDevice {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl BusDevice<Gpa> for BlockingReadDevice {
        fn read(&self, offset: u64, data: &mut [u8]) {
            assert_eq!(offset, 0);
            data.copy_from_slice(&MMIO_READ_VALUE.to_le_bytes());
            self.entered.send(()).unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(TIMEOUT)
                .expect("timed out waiting to release the MMIO read");
        }

        fn write(&self, _offset: u64, _data: &[u8]) {
            panic!("unexpected MMIO write");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exit_notification_timeout_preserves_receiver_and_thread_for_retry() {
        let (release, released) = mpsc::channel();
        let (notice, exited) = oneshot::channel();
        let thread = thread::spawn(move || {
            released.recv_timeout(TIMEOUT).unwrap();
            let _ = notice.send(());
        });
        let mut pending = VcpuThread {
            exited: Some(exited),
            owner: ThreadOwner::Running(thread),
        };
        assert!(matches!(
            pending.join_wait(3, Instant::now()).await,
            Err(VcpuError::TimedOut(3))
        ));
        assert!(pending.exited.is_some());
        assert!(matches!(pending.owner, ThreadOwner::Running(_)));
        assert!(pending.is_alive());
        release.send(()).unwrap();
        pending
            .join_wait(3, Instant::now() + TIMEOUT)
            .await
            .unwrap();
        assert!(pending.is_joined());
        assert!(pending.exited.is_none());
        pending.join_wait(3, Instant::now()).await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_pool_join_timeout_retains_the_actual_join_task() {
        let (release, released) = mpsc::channel();
        let (ready, notified) = mpsc::channel();
        let (notice, exited) = oneshot::channel();
        let thread = thread::spawn(move || {
            notice.send(()).unwrap();
            ready.send(()).unwrap();
            // The notice is not the OS join: retain the real handle until return.
            released.recv_timeout(TIMEOUT).unwrap();
        });
        notified.recv_timeout(TIMEOUT).unwrap();
        let mut pending = VcpuThread {
            exited: Some(exited),
            owner: ThreadOwner::Running(thread),
        };
        assert!(
            !pending.is_alive(),
            "an exit notice invalidates an old Pause ack"
        );
        assert!(matches!(
            pending.join_wait(4, Instant::now()).await,
            Err(VcpuError::TimedOut(4))
        ));
        assert!(pending.exited.is_none());
        assert!(matches!(pending.owner, ThreadOwner::Joining(_)));
        assert!(!pending.is_joined());
        release.send(()).unwrap();
        pending
            .join_wait(4, Instant::now() + TIMEOUT)
            .await
            .unwrap();
        assert!(pending.is_joined());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_panicked_thread_is_joined_even_when_its_exit_sender_was_dropped() {
        let (notice, exited) = oneshot::channel();
        let thread = thread::spawn(move || {
            let _notice = notice;
            panic!("regression: panic before sending the exit notice");
        });
        let mut pending = VcpuThread {
            exited: Some(exited),
            owner: ThreadOwner::Running(thread),
        };
        assert!(matches!(
            pending.join_wait(5, Instant::now() + TIMEOUT).await,
            Err(VcpuError::Panicked(5))
        ));
        assert!(
            pending.is_joined(),
            "a panic must not retain completed ownership"
        );
        assert!(!pending.is_alive());
        pending.join_wait(5, Instant::now()).await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fresh_vcpu_roundtrips_with_and_without_registered_pvclock_on_kvm() {
        use crate::machine::kvm_state;

        let Some(context) = kvm() else { return };
        let supported = msr::snapshot_msr_indices(&context.capabilities().supported_cpuid);
        for registration in [
            None,
            Some((msr::MSR_KVM_SYSTEM_TIME, msr::MSR_KVM_WALL_CLOCK)),
            Some((msr::MSR_KVM_SYSTEM_TIME_NEW, msr::MSR_KVM_WALL_CLOCK_NEW)),
        ] {
            if registration.is_some_and(|(system, _)| !supported.contains(&system)) {
                continue;
            }
            for kind in [BackingKind::AnonymousPrivate, BackingKind::SharedMemfd] {
                let memory = store(&context, MemoryLayout::boot(16 << 20).unwrap(), kind);
                let (vm, bindings, slots) = create_vm(&context, memory).unwrap();
                let view = Arc::downgrade(vm.memory().view());
                let cpu = bsp(&context, &vm, &[0xf4]);
                if let Some((system, wall)) = registration {
                    let entries = [
                        kvm_msr_entry {
                            index: wall,
                            data: 0x3000,
                            ..Default::default()
                        },
                        kvm_msr_entry {
                            index: system,
                            data: 0x2001,
                            ..Default::default()
                        },
                    ];
                    restore_msrs(&cpu, 0, &entries, RestoreStep::Msrs).unwrap();
                }
                let (events, _receiver) = tokio_mpsc::unbounded_channel();
                let mut vcpus = Vcpus::spawn_retained(vec![cpu], Arc::default(), events).unwrap();
                let saved = vcpus.capture_state(vm.fd()).unwrap().pop().unwrap();
                let vm_state = kvm_state::capture_vm_kvm(vm.fd()).unwrap();
                assert!(msr::snapshot_msrs_match(&saved.cpuid, &saved.msrs));
                assert!(saved.ssp_matches_cpuid());
                assert_eq!(msr::pvclock_enabled(&saved.msrs), registration.is_some());
                vcpus.stop_wait(TIMEOUT).await.unwrap();
                drop((bindings, slots));
                let memory = vm.into_memory().unwrap();
                assert!(Arc::ptr_eq(memory.view(), &view.upgrade().unwrap()));

                let (vm, _bindings, _slots) = create_vm(&context, memory).unwrap();
                kvm_state::restore_vm_kvm(vm.fd(), &vm_state).unwrap();
                let fresh = vm.create_vcpu(0).unwrap();
                assert_eq!(fresh.kvmclock_ctrl().unwrap_err().errno(), libc::EINVAL);
                restore_state(&fresh, vm.fd(), &saved).unwrap();
                if let Some(ssp) = saved.ssp {
                    let mut data = [0; mem::size_of::<u64>()];
                    fresh
                        .get_one_reg(kvm_x86_reg_kvm(KVM_REG_GUEST_SSP), &mut data)
                        .unwrap();
                    assert_eq!(u64::from_ne_bytes(data), ssp);
                }
                kvm_state::restore_vm_clock(vm.fd(), &vm_state).unwrap();
                restore_wall_clock(&fresh, &saved).unwrap();
                assert!(Arc::ptr_eq(vm.memory().view(), &view.upgrade().unwrap()));
                if registration.is_some() {
                    fresh.kvmclock_ctrl().unwrap();
                } else {
                    assert_eq!(fresh.kvmclock_ctrl().unwrap_err().errno(), libc::EINVAL);
                }
                let mut query = Msrs::from_entries(&saved.msrs).unwrap();
                assert_eq!(fresh.get_msrs(&mut query).unwrap(), saved.msrs.len());
                let clocks = msr::wall_clock_msrs(&saved.msrs).chain(
                    saved.msrs.iter().copied().filter(|entry| {
                        matches!(
                            entry.index,
                            msr::MSR_KVM_SYSTEM_TIME | msr::MSR_KVM_SYSTEM_TIME_NEW
                        )
                    }),
                );
                for before in clocks {
                    let after = query
                        .as_slice()
                        .iter()
                        .find(|entry| entry.index == before.index)
                        .unwrap();
                    assert_eq!(after.data, before.data);
                }
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn capture_rejects_unknown_or_exited_cpus_despite_an_old_pause_ack_on_kvm() {
        let Some(context) = kvm() else { return };
        let (vm, _bindings) = boot_vm(&context);
        let vcpu = bsp(&context, &vm, &[0xf4]);
        let (events, _receiver) = tokio_mpsc::unbounded_channel();
        let mut vcpus = Vcpus::spawn_retained(vec![vcpu], Arc::default(), events).unwrap();
        vcpus.pause(TIMEOUT).await.unwrap();
        vcpus.0[0].state = None;
        assert!(matches!(
            vcpus.capture_state(vm.fd()),
            Err(CaptureVcpuError::NotPaused(0))
        ));
        vcpus.0[0].state = Some(State::Paused);
        // Direct Stop preserves the stale ack deliberately; a parked thread needs no kick.
        vcpus.0[0].commands.send(VcpuCommand::Stop).unwrap();
        vcpus.0[0]
            .thread
            .join_wait(0, Instant::now() + TIMEOUT)
            .await
            .unwrap();
        assert!(matches!(
            vcpus.capture_state(vm.fd()),
            Err(CaptureVcpuError::Exited(0))
        ));
        vcpus.stop_wait(TIMEOUT).await.unwrap();
        assert_eq!(vcpus.len(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mixed_closed_and_unanswered_replies_prioritize_timeout() {
        for closed_first in [true, false] {
            let (closed, exited) = oneshot::channel();
            drop(closed);
            let (held, unanswered) = oneshot::channel();
            let answers = if closed_first {
                vec![(0, exited), (1, unanswered)]
            } else {
                vec![(1, unanswered), (0, exited)]
            };
            let result = time::timeout(
                Duration::from_secs(1),
                wait_for_replies(answers, Instant::now() + Duration::from_millis(10)),
            )
            .await
            .expect("reply wait exceeded its shared deadline");
            assert!(matches!(result, Err(VcpuError::TimedOut(1))));
            drop(held);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn every_reply_must_acknowledge_before_the_barrier_completes() {
        use std::future::{Future, poll_fn};
        use std::pin::pin;
        use std::task::Poll;

        let (first, first_answer) = oneshot::channel();
        let (held, held_answer) = oneshot::channel();
        first.send(()).unwrap();
        let mut waiting = pin!(wait_for_replies(
            vec![(0, first_answer), (1, held_answer)],
            Instant::now() + TIMEOUT,
        ));
        assert!(poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx).is_pending())).await);
        held.send(()).unwrap();
        time::timeout(TIMEOUT, waiting).await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pause_waits_for_mmio_read_completion_on_kvm() {
        let Some(context) = kvm() else { return };
        let (vm, bindings) = boot_vm(&context);
        let code = [
            0xa1, 0x00, 0x00, 0x00, 0x10, // mov eax, [0x1000_0000]
            0x41, // inc ecx: must not run before Pause is acknowledged
            0xf4, // hlt
            0xeb, 0xfd, // jmp back to hlt
        ];
        let vcpu = bsp(&context, &vm, &code);
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let device = BlockingReadDevice {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        };
        let mmio = MmioBus::new(vec![(
            Range::new(Gpa(0x1000_0000), 4).unwrap(),
            Arc::new(device) as Arc<dyn BusDevice<Gpa>>,
        )])
        .unwrap();
        let buses = Arc::new(Buses {
            pio: PioBus::default(),
            mmio,
        });
        let (events, mut receiver) = tokio_mpsc::unbounded_channel();
        let mut vcpus = Vcpus::spawn_retained(vec![vcpu], buses, events).unwrap();

        vcpus.resume(TIMEOUT).await.unwrap();
        entered.recv_timeout(TIMEOUT).expect("the guest never read");
        // Queue Pause while the read still owns KVM's pending MMIO data.
        let handle = &mut vcpus.0[0];
        let (reply, paused) = oneshot::channel();
        handle.command(VcpuCommand::Pause(reply));
        release.send(()).unwrap();
        time::timeout(TIMEOUT, paused).await.unwrap().unwrap();

        let regs = handle.kick.get_regs().unwrap();
        assert_eq!(regs.rax, u64::from(MMIO_READ_VALUE));
        assert_eq!(regs.rip, ENTRY.0 + 5);
        assert_eq!(regs.rcx, 0, "the instruction after the MMIO read ran");

        vcpus.stop_wait(TIMEOUT).await.unwrap();
        assert_eq!(vcpus.len(), 0);
        assert!(receiver.try_recv().is_err(), "the vCPU reported an event");
        // Stopped vCPUs hold nothing: the VM gives its memory back.
        drop(bindings);
        assert!(KvmVm::into_memory(vm).is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn protected_mode_payload_uses_pvh_serial_and_i8042_on_kvm() {
        let Some(context) = kvm() else { return };
        let (vm, _bindings) = boot_vm(&context);
        const PVH_MAGIC: u32 = 0x336e_c578;
        let code = [
            0x8b, 0x03, // mov eax, [ebx]
            0x3d, 0x78, 0xc5, 0x6e, 0x33, // cmp eax, PVH_MAGIC
            0x75, 0x14, // jne fail
            0x66, 0xba, 0xf8, 0x03, // mov dx, 0x3f8
            0xb0, b'O', 0xee, // out dx, al
            0xb0, b'K', 0xee, // out dx, al
            0x66, 0xba, 0x64, 0x00, // mov dx, 0x64
            0xb0, 0xfe, 0xee, // out dx, al: i8042 reset
            0xf4, // hlt while teardown catches up
            0xeb, 0xfd, // jmp back to hlt
            0x66, 0xba, 0xf8, 0x03, // fail: mov dx, 0x3f8
            0xb0, b'F', 0xee, // out dx, al
            0x0f, 0x0b, // ud2
        ];
        vm.memory()
            .view()
            .write_obj(PVH_MAGIC, GuestAddress(PVH_INFO_START.0))
            .unwrap();
        let vcpu = bsp(&context, &vm, &code);

        let output = Captured::default();
        let (_serial, port) = SerialDevice::new(None, output.clone()).unwrap();
        vm.fd().register_irqfd(port.irq_fd(), COM1_GSI.0).unwrap();
        let (events, mut receiver) = tokio_mpsc::unbounded_channel();
        let pio = PioBus::new(vec![
            (COM1_PORTS, Arc::new(port) as Arc<dyn BusDevice<Pio>>),
            (I8042_PORTS, Arc::new(I8042Device::new(events.clone()))),
        ])
        .unwrap();
        let buses = Arc::new(Buses {
            pio,
            mmio: MmioBus::default(),
        });
        let mut vcpus = Vcpus::spawn_retained(vec![vcpu], buses, events).unwrap();

        // Pausing parked vCPUs is idempotent.
        vcpus.pause(TIMEOUT).await.unwrap();
        vcpus.resume(TIMEOUT).await.unwrap();
        let event = time::timeout(TIMEOUT, receiver.recv()).await.unwrap();
        assert_eq!(event, Some(MachineEvent::GuestReset));
        assert_eq!(*output.0.lock().unwrap(), b"OK");
        vcpus.stop_wait(TIMEOUT).await.unwrap();
        assert_eq!(vcpus.len(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn running_guest_observes_runtime_bind_unbind_and_drop_on_kvm() {
        let Some(context) = kvm() else { return };
        const CHUNK: Gpa = Gpa(0x1000_1000);
        const VALUE: u64 = 0x8000;
        const SEQUENCE: u64 = 0x8004;
        const MARKER: u32 = 0x4433_2211;
        let window = |object, len, start| {
            Window::new(
                ObjectId(object),
                Range::new(ObjectOffset(0), len).unwrap(),
                start,
                WindowOwner::Boot,
                WindowKind::BootRam,
            )
            .unwrap()
        };
        // 16 MiB of boot RAM, and one page of another object bound at runtime.
        let layout = MemoryLayout::new(
            [
                (ObjectId(0), Object { capacity: 16 << 20 }),
                (ObjectId(1), Object { capacity: 0x1000 }),
            ],
            [
                (WindowId(0), window(0, 16 << 20, Gpa(0))),
                (WindowId(1), window(1, 0x1000, CHUNK)),
            ],
        )
        .unwrap();
        let mut code = vec![0xa1]; // mov eax, [CHUNK]
        code.extend_from_slice(&(CHUNK.0 as u32).to_le_bytes());
        code.push(0xa3); // mov [VALUE], eax
        code.extend_from_slice(&(VALUE as u32).to_le_bytes());
        code.extend_from_slice(&[0xff, 0x05]); // inc dword [SEQUENCE]
        code.extend_from_slice(&(SEQUENCE as u32).to_le_bytes());
        code.extend_from_slice(&[0xeb, 0xee]); // jmp back to the start

        for kind in [BackingKind::AnonymousPrivate, BackingKind::SharedMemfd] {
            let memory = store(&context, layout.clone(), kind);
            let (vm, mut slots) = KvmVm::new(&context, memory).unwrap();
            let mut permits = slots.allocate(2).unwrap();
            let permit = permits.pop().unwrap();
            let _boot = permits.pop().unwrap().bind(WindowId(0)).unwrap();
            let view = vm.memory().view();
            view.write_obj(MARKER, GuestAddress(CHUNK.0)).unwrap();
            let vcpu = bsp(&context, &vm, &code);
            let (events, mut receiver) = tokio_mpsc::unbounded_channel();
            let mut vcpus = Vcpus::spawn_retained(vec![vcpu], Arc::default(), events).unwrap();

            // Waits for the guest to make progress and read `expected`.
            let observe = |expected: u32| async move {
                let before = view.read_obj::<u32>(GuestAddress(SEQUENCE)).unwrap();
                time::timeout(TIMEOUT, async {
                    loop {
                        let sequence = view.read_obj::<u32>(GuestAddress(SEQUENCE)).unwrap();
                        let value = view.read_obj::<u32>(GuestAddress(VALUE)).unwrap();
                        if sequence != before && value == expected {
                            return;
                        }
                        task::yield_now().await;
                    }
                })
                .await
                .expect("the guest stopped or read the wrong value");
            };
            vcpus.resume(TIMEOUT).await.unwrap();
            // Unbound, the chunk is MMIO that no device answers.
            observe(0).await;
            let binding = permit.bind(WindowId(1)).unwrap();
            observe(MARKER).await;
            let (permit, _mapping) = binding.unbind().unwrap();
            observe(0).await;
            let binding = permit.bind(WindowId(1)).unwrap();
            observe(MARKER).await;
            drop(binding);
            observe(0).await;
            vcpus.stop_wait(TIMEOUT).await.unwrap();
            assert_eq!(vcpus.len(), 0);
            assert!(receiver.try_recv().is_err(), "the guest or its vCPU failed");
        }
    }
}
