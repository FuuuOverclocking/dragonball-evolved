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
    KVM_MP_STATE_UNINITIALIZED, KVM_SYSTEM_EVENT_RESET, KVM_SYSTEM_EVENT_SHUTDOWN, Msrs,
    kvm_mp_state,
};
use kvm_ioctls::VcpuExit;
use logger::{error_unlimited, info, warn_unlimited};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use tokio::task;
use tokio::time::{self, Instant};
use vm_memory::{Bytes, GuestAddress, GuestMemoryError};
use vmm_sys_util::signal::{SIGRTMIN, register_signal_handler};
use vmm_sys_util::{errno, fam};

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
    /// Starts one parked thread per vCPU; `vcpus[i]` is vCPU `i`.
    pub(super) fn spawn(
        vcpus: Vec<KvmVcpu>,
        buses: Arc<Buses>,
        events: tokio_mpsc::UnboundedSender<MachineEvent>,
    ) -> Result<Self, SpawnVcpuError> {
        install_kick_handler()?;
        let mut handles = Vec::with_capacity(vcpus.len());
        for (index, fd) in (0..=u8::MAX).zip(vcpus) {
            // Handles already spawned stop their threads when dropped.
            handles.push(VcpuHandle::spawn(
                index,
                fd,
                Arc::clone(&buses),
                events.clone(),
            )?);
        }
        Ok(Self(handles))
    }

    pub(super) fn len(&self) -> usize {
        self.0.len()
    }

    /// Asks every vCPU to pause, and waits at most `within` for all of them.
    /// Only an acknowledgement proves the pause barrier was reached; a queued
    /// command and its kick alone do not prove that the vCPU is parked.
    pub(super) async fn pause(&mut self, within: Duration) -> Result<(), VcpuError> {
        self.request(VcpuCommand::Pause, within).await
    }

    /// Asks every vCPU to run, and waits at most `within` for all of them.
    pub(super) async fn resume(&mut self, within: Duration) -> Result<(), VcpuError> {
        self.request(VcpuCommand::Resume, within).await
    }

    /// Stops every vCPU and joins its thread, waiting at most `within`. A
    /// thread that does not end in time is left to end on its own, holding
    /// its VM, and so the guest memory, until it does.
    pub(super) async fn stop(mut self, within: Duration) -> Result<(), VcpuError> {
        let deadline = Instant::now() + within;
        for handle in &mut self.0 {
            handle.command(VcpuCommand::Stop);
        }
        let mut first = None;
        for handle in mem::take(&mut self.0) {
            if let Err(error) = handle.join(deadline).await {
                warn_unlimited!("{}", render_chain(&error));
                first.get_or_insert(error);
            }
        }
        first.map_or(Ok(()), Err)
    }

    async fn request(
        &mut self,
        command: fn(oneshot::Sender<()>) -> VcpuCommand,
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
        wait_for_replies(answers, deadline).await
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
    exited: Option<oneshot::Receiver<()>>,
    // Held until joined, which keeps the thread's pthread_t valid for kicks.
    thread: Option<thread::JoinHandle<()>>,
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
            exited: Some(exited),
            thread: Some(thread),
        })
    }

    /// Queues `command`, then kicks. A thread that has ended has dropped its
    /// receiver, and the reply slot goes with the command.
    fn command(&mut self, command: VcpuCommand) {
        if self.commands.send(command).is_ok() {
            self.kick();
        }
    }

    fn kick(&mut self) {
        self.kick.set_kvm_immediate_exit(1);
        fence(Ordering::SeqCst);
        let Some(thread) = &self.thread else {
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

    /// Waits until `deadline` for the thread to end, then joins it on the
    /// blocking pool.
    async fn join(mut self, deadline: Instant) -> Result<(), VcpuError> {
        let index = self.index;
        if let Some(exited) = self.exited.take()
            && time::timeout_at(deadline, exited).await.is_err()
        {
            return Err(VcpuError::TimedOut(index));
        }
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        match time::timeout_at(deadline, task::spawn_blocking(move || thread.join())).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(_))) => Err(VcpuError::Panicked(index)),
            Ok(Err(source)) => Err(VcpuError::Join { index, source }),
            Err(_) => Err(VcpuError::TimedOut(index)),
        }
    }
}

impl Drop for VcpuHandle {
    fn drop(&mut self) {
        if self.thread.is_some() {
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
        let mut vcpus = Vcpus::spawn(vec![vcpu], buses, events).unwrap();

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

        vcpus.stop(TIMEOUT).await.unwrap();
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
        let mut vcpus = Vcpus::spawn(vec![vcpu], buses, events).unwrap();

        // Pausing parked vCPUs is idempotent.
        vcpus.pause(TIMEOUT).await.unwrap();
        vcpus.resume(TIMEOUT).await.unwrap();
        let event = time::timeout(TIMEOUT, receiver.recv()).await.unwrap();
        assert_eq!(event, Some(MachineEvent::GuestReset));
        assert_eq!(*output.0.lock().unwrap(), b"OK");
        vcpus.stop(TIMEOUT).await.unwrap();
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
            let mut vcpus = Vcpus::spawn(vec![vcpu], Arc::default(), events).unwrap();

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
            vcpus.stop(TIMEOUT).await.unwrap();
            assert!(receiver.try_recv().is_err(), "the guest or its vCPU failed");
        }
    }
}
