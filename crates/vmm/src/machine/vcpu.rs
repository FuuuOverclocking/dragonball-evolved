use std::os::unix::thread::JoinHandleExt;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{Ordering, fence};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use kvm_bindings::{
    KVM_MAX_CPUID_ENTRIES, KVM_SYSTEM_EVENT_RESET, KVM_SYSTEM_EVENT_SHUTDOWN, Msrs, kvm_msr_entry,
};
use kvm_ioctls::{VcpuExit, VcpuFd};
use logger::warn;
use tokio::sync::mpsc as tokio_mpsc;
use vmm_sys_util::signal::{SIGRTMIN, register_signal_handler};

use super::MachineEvent;
use super::bus::Bus;
use super::snapshot::VcpuSnapshot;
use super::vm::KvmVcpu;
use crate::arch::x86_64::msr::msr_indices;

static SIGNAL_HANDLER_ERRNO: OnceLock<i32> = OnceLock::new();

extern "C" fn kick_signal_handler(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
    fence(Ordering::Acquire);
}

pub enum VcpuCommand {
    Pause(oneshot::Sender<()>),
    Resume(oneshot::Sender<()>),
    Stop,
}

pub struct VcpuHandle {
    index: u8,
    kick_fd: Mutex<KvmVcpu>,
    commands: mpsc::Sender<VcpuCommand>,
    exited: Option<oneshot::Receiver<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

pub struct Vcpus {
    handles: Vec<VcpuHandle>,
}

struct VcpuRunner {
    index: u8,
    fd: KvmVcpu,
    pio: Arc<Bus>,
    mmio: Arc<Bus>,
    commands: mpsc::Receiver<VcpuCommand>,
    events: tokio_mpsc::UnboundedSender<MachineEvent>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RunState {
    Paused,
    Running,
    Stop,
}

enum RunResult {
    Handled,
    Interrupted,
    Stop,
}

impl VcpuHandle {
    pub fn spawn(
        index: u8,
        fd: KvmVcpu,
        pio: Arc<Bus>,
        mmio: Arc<Bus>,
        events: tokio_mpsc::UnboundedSender<MachineEvent>,
    ) -> Result<Self> {
        install_signal_handler()?;
        let kick_fd = fd.try_clone()?;
        let (command_tx, command_rx) = mpsc::channel();
        let (exited_tx, exited_rx) = oneshot::channel();
        let panic_events = events.clone();
        let runner = VcpuRunner {
            index,
            fd,
            pio,
            mmio,
            commands: command_rx,
            events,
        };
        let thread = thread::Builder::new()
            .name(format!("vcpu-{index}"))
            .spawn(move || {
                if let Err(error) = unblock_kick_signal() {
                    let _ = panic_events.send(MachineEvent::VcpuFailed {
                        index,
                        error: error.to_string(),
                    });
                    let _ = exited_tx.send(());
                    return;
                }
                if panic::catch_unwind(AssertUnwindSafe(|| runner.run())).is_err() {
                    let _ = panic_events.send(MachineEvent::VcpuFailed {
                        index,
                        error: "vCPU thread panicked".into(),
                    });
                }
                let _ = exited_tx.send(());
            })
            .context("spawn vCPU thread")?;
        Ok(Self {
            index,
            kick_fd: Mutex::new(kick_fd),
            commands: command_tx,
            exited: Some(exited_rx),
            thread: Some(thread),
        })
    }

    pub fn capture_state(&self) -> Result<VcpuSnapshot> {
        let fd = self
            .kick_fd
            .lock()
            .map_err(|_| anyhow!("vCPU {} kick fd mutex was poisoned", self.index))?;
        // Ordering: get_mp_state first because the internal kvm_apic_accept_events
        // can modify LAPIC state.
        let mp_state = fd.get_mp_state().context("get mp_state")?;
        let regs = fd.get_regs().context("get regs")?;
        let sregs = fd.get_sregs().context("get sregs")?;
        let xsave = fd.get_xsave().context("get xsave")?;
        let lapic = fd.get_lapic().context("get lapic")?;
        let cpuid = fd.get_cpuid2(KVM_MAX_CPUID_ENTRIES).context("get cpuid")?;

        // Build MSR query from known indices.
        let entries: Vec<kvm_msr_entry> = msr_indices()
            .into_iter()
            .map(|index| kvm_msr_entry {
                index,
                ..Default::default()
            })
            .collect();
        let mut msrs_req = Msrs::from_entries(&entries).context("build MSR query")?;
        let count = fd.get_msrs(&mut msrs_req).context("get msrs")?;
        let msrs = msrs_req.as_slice()[..count].to_vec();

        let xcrs = fd.get_xcrs().context("get xcrs")?;
        let debug_regs = fd.get_debug_regs().context("get debug_regs")?;
        let vcpu_events = fd.get_vcpu_events().context("get vcpu_events")?;

        Ok(VcpuSnapshot {
            index: self.index,
            mp_state,
            regs,
            sregs,
            xsave,
            lapic,
            cpuid,
            msrs,
            xcrs,
            debug_regs,
            vcpu_events,
        })
    }

    fn command(&self, command: VcpuCommand) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| anyhow!("vCPU {} already exited", self.index))?;
        self.kick()
    }

    fn kick(&self) -> Result<()> {
        self.kick_fd
            .lock()
            .map_err(|_| anyhow!("vCPU {} kick fd mutex was poisoned", self.index))?
            .set_kvm_immediate_exit(1);
        fence(Ordering::Release);
        let thread = self
            .thread
            .as_ref()
            .context("vCPU thread was already joined")?;
        // SAFETY: The pthread handle remains valid until its JoinHandle is joined.
        let errno = unsafe { libc::pthread_kill(thread.as_pthread_t(), SIGRTMIN()) };
        if errno != 0 {
            return Err(std::io::Error::from_raw_os_error(errno)).context("signal vCPU");
        }
        Ok(())
    }

    async fn wait(mut self) -> Result<()> {
        if let Some(exited) = self.exited.take() {
            exited
                .await
                .map_err(|_| anyhow!("vCPU {} exit notification was dropped", self.index))?;
        }
        if let Some(thread) = self.thread.take() {
            tokio::task::spawn_blocking(move || {
                thread.join().map_err(|_| anyhow!("vCPU thread panicked"))
            })
            .await??;
        }
        Ok(())
    }

    fn join(&mut self) -> Result<()> {
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow!("vCPU {} thread panicked", self.index))?;
        }
        Ok(())
    }
}

/// Restores state taken by [`VcpuHandle::capture_state`] onto a fresh vCPU.
pub(super) fn restore_state(fd: &VcpuFd, snapshot: &VcpuSnapshot) -> Result<()> {
    // Restore ordering follows Firecracker: cpuid first (BSP detection
    // depends on it), then mp_state, general regs, sregs, xsave, xcrs,
    // debug regs, lapic (after sregs), MSRs, vcpu_events last.
    fd.set_cpuid2(&snapshot.cpuid).context("restore cpuid")?;
    fd.set_mp_state(snapshot.mp_state)
        .context("restore mp_state")?;
    fd.set_regs(&snapshot.regs).context("restore regs")?;
    fd.set_sregs(&snapshot.sregs).context("restore sregs")?;
    // SAFETY: We restore the same kvm_xsave that get_xsave() produced on the
    // same host. No dynamic XSTATE features are enabled, so the traditional
    // 4096-byte kvm_xsave is sufficient.
    unsafe { fd.set_xsave(&snapshot.xsave) }.context("restore xsave")?;
    fd.set_xcrs(&snapshot.xcrs).context("restore xcrs")?;
    fd.set_debug_regs(&snapshot.debug_regs)
        .context("restore debug_regs")?;
    fd.set_lapic(&snapshot.lapic).context("restore lapic")?;
    let msrs = Msrs::from_entries(&snapshot.msrs).context("build MSR set")?;
    let set_count = fd.set_msrs(&msrs).context("restore msrs")?;
    if set_count != snapshot.msrs.len() {
        bail!(
            "KVM set {set_count}/{} MSRs for vCPU {}",
            snapshot.msrs.len(),
            snapshot.index
        );
    }
    fd.set_vcpu_events(&snapshot.vcpu_events)
        .context("restore vcpu_events")?;
    // Notify KVM that the guest's pvclock may be out of sync after restore,
    // preventing spurious soft-lockup detection inside the guest.
    if let Err(error) = fd.kvmclock_ctrl() {
        warn!(
            "kvmclock_ctrl failed for vCPU {}: {error:#}",
            snapshot.index
        );
    }
    Ok(())
}

impl Drop for VcpuHandle {
    fn drop(&mut self) {
        if self.thread.is_none() {
            return;
        }
        let _ = self.commands.send(VcpuCommand::Stop);
        let _ = self.kick();
        let _ = self.join();
    }
}

impl Vcpus {
    pub fn new(handles: Vec<VcpuHandle>) -> Self {
        Self { handles }
    }

    pub fn handles(&self) -> &[VcpuHandle] {
        &self.handles
    }

    pub fn len(&self) -> usize {
        self.handles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    pub async fn pause(&self) -> Result<()> {
        let mut replies = Vec::with_capacity(self.handles.len());
        for handle in &self.handles {
            let (reply, waiter) = oneshot::channel();
            handle.command(VcpuCommand::Pause(reply))?;
            replies.push(waiter);
        }
        wait_for_replies(replies, "pause").await
    }

    pub async fn resume(&self) -> Result<()> {
        let mut replies = Vec::with_capacity(self.handles.len());
        for handle in &self.handles {
            let (reply, waiter) = oneshot::channel();
            handle.command(VcpuCommand::Resume(reply))?;
            replies.push(waiter);
        }
        wait_for_replies(replies, "resume").await
    }

    pub async fn stop(self) -> Result<()> {
        for handle in &self.handles {
            let _ = handle.command(VcpuCommand::Stop);
        }
        for handle in self.handles {
            handle.wait().await?;
        }
        Ok(())
    }
}

async fn wait_for_replies(replies: Vec<oneshot::Receiver<()>>, operation: &str) -> Result<()> {
    for reply in replies {
        reply
            .await
            .map_err(|_| anyhow!("vCPU dropped {operation} reply"))?;
    }
    Ok(())
}

impl VcpuRunner {
    fn run(mut self) {
        let mut state = RunState::Paused;
        loop {
            state = match state {
                RunState::Paused => match self.commands.recv() {
                    Ok(command) => {
                        self.fd.set_kvm_immediate_exit(0);
                        apply_command(command)
                    }
                    Err(_) => RunState::Stop,
                },
                RunState::Running => match self.run_once() {
                    RunResult::Handled => RunState::Running,
                    RunResult::Interrupted => {
                        self.fd.set_kvm_immediate_exit(0);
                        self.pending_commands(RunState::Running)
                    }
                    RunResult::Stop => RunState::Stop,
                },
                RunState::Stop => return,
            };
        }
    }

    fn run_once(&mut self) -> RunResult {
        match self.fd.run() {
            Ok(VcpuExit::IoIn(port, data)) => {
                data.fill(0);
                self.pio.read(u64::from(port), data);
                RunResult::Handled
            }
            Ok(VcpuExit::IoOut(port, data)) => {
                self.pio.write(u64::from(port), data);
                RunResult::Handled
            }
            Ok(VcpuExit::MmioRead(address, data)) => {
                data.fill(0);
                self.mmio.read(address, data);
                RunResult::Handled
            }
            Ok(VcpuExit::MmioWrite(address, data)) => {
                self.mmio.write(address, data);
                RunResult::Handled
            }
            Ok(VcpuExit::Shutdown)
            | Ok(VcpuExit::SystemEvent(KVM_SYSTEM_EVENT_RESET | KVM_SYSTEM_EVENT_SHUTDOWN, _)) => {
                let _ = self.events.send(MachineEvent::GuestReset);
                RunResult::Stop
            }
            Ok(VcpuExit::Intr) => RunResult::Interrupted,
            Ok(exit) => report_failure(
                &self.events,
                self.index,
                format!("unexpected KVM exit: {exit:?}"),
            ),
            Err(error) if error.errno() == libc::EINTR => RunResult::Interrupted,
            Err(error) if error.errno() == libc::EAGAIN => RunResult::Handled,
            Err(error) => {
                report_failure(&self.events, self.index, format!("KVM_RUN failed: {error}"))
            }
        }
    }

    fn pending_commands(&mut self, mut state: RunState) -> RunState {
        loop {
            match self.commands.try_recv() {
                Ok(command) => {
                    state = apply_command(command);
                    if state == RunState::Stop {
                        return state;
                    }
                }
                Err(TryRecvError::Empty) => return state,
                Err(TryRecvError::Disconnected) => return RunState::Stop,
            }
        }
    }
}

fn report_failure(
    events: &tokio_mpsc::UnboundedSender<MachineEvent>,
    index: u8,
    error: String,
) -> RunResult {
    let _ = events.send(MachineEvent::VcpuFailed { index, error });
    RunResult::Stop
}

fn apply_command(command: VcpuCommand) -> RunState {
    match command {
        VcpuCommand::Pause(reply) => {
            let _ = reply.send(());
            RunState::Paused
        }
        VcpuCommand::Resume(reply) => {
            let _ = reply.send(());
            RunState::Running
        }
        VcpuCommand::Stop => RunState::Stop,
    }
}

fn unblock_kick_signal() -> Result<()> {
    // SAFETY: sigemptyset initializes the mask; all pointers live through their calls.
    let errno = unsafe {
        let mut mask = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        libc::sigemptyset(mask.as_mut_ptr());
        let mut mask = mask.assume_init();
        libc::sigaddset(&mut mask, SIGRTMIN());
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut())
    };
    if errno != 0 {
        return Err(std::io::Error::from_raw_os_error(errno)).context("unblock vCPU kick signal");
    }
    Ok(())
}

fn install_signal_handler() -> Result<()> {
    let errno = *SIGNAL_HANDLER_ERRNO.get_or_init(|| {
        register_signal_handler(SIGRTMIN(), kick_signal_handler)
            .err()
            .map_or(0, |error| error.errno())
    });
    if errno == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(errno)).context("install vCPU kick signal handler")
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::path::Path;
    use std::range::Range;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use kvm_bindings::Msrs;
    use tokio::time;
    use vm_memory::{Bytes, GuestAddress};

    use super::*;
    use crate::KvmContext;
    use crate::arch::x86_64::cpuid::normalize;
    use crate::arch::x86_64::lapic::configure_lint;
    use crate::arch::x86_64::layout::{COM1_GSI, COM1_PORTS, I8042_PORTS, ram_regions};
    use crate::arch::x86_64::msr::boot_msrs;
    use crate::arch::x86_64::regs::{boot_fpu, boot_regs, boot_sregs};
    use crate::devices::legacy::i8042::I8042Device;
    use crate::devices::legacy::serial::SerialDevice;
    use crate::machine::bus::BusDevice;
    use crate::machine::memory::{self, MemoryPlan};
    use crate::machine::vm::KvmVm;

    const KVM_TEST_TIMEOUT: Duration = Duration::from_secs(2);
    const MMIO_READ_VALUE: u32 = 0x4433_2211;

    fn test_vm(context: &KvmContext) -> Arc<KvmVm> {
        let plan = MemoryPlan {
            backing: api::machine::MemoryBacking::Anonymous,
            regions: ram_regions(16 << 20).unwrap(),
        };
        KvmVm::new(context, memory::create(&plan).unwrap()).unwrap()
    }

    #[derive(Clone)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct BlockingReadDevice {
        entered: std::sync::mpsc::Sender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl BusDevice for BlockingReadDevice {
        fn read(&self, offset: u64, data: &mut [u8]) {
            assert_eq!(offset, 0);
            assert_eq!(data.len(), std::mem::size_of::<u32>());
            data.copy_from_slice(&MMIO_READ_VALUE.to_le_bytes());
            self.entered.send(()).unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(KVM_TEST_TIMEOUT)
                .expect("timed out waiting to release MMIO read");
        }

        fn write(&self, _offset: u64, _data: &[u8]) {
            panic!("unexpected MMIO write");
        }
    }

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pause_waits_for_mmio_read_completion_on_kvm() {
        if !Path::new("/dev/kvm").exists() {
            return;
        }
        let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
        let vm = test_vm(&context);
        let memory = Arc::clone(vm.memory());

        const ENTRY: u64 = 0x10_0000;
        const MMIO_ADDRESS: u64 = 0x1000_0000;
        let code = [
            0xa1, 0x00, 0x00, 0x00, 0x10, // mov eax, [MMIO_ADDRESS]
            0x41, // inc ecx: must not execute before Pause is acknowledged
            0xf4, // hlt
            0xeb, 0xfd, // jmp back to hlt
        ];
        memory.write_slice(&code, GuestAddress(ENTRY)).unwrap();

        let vcpu = vm.create_vcpu(0).unwrap();
        let cpuid = normalize(
            &context.capabilities().supported_cpuid,
            &context.capabilities().host_cpuid,
            1,
            0,
        )
        .unwrap();
        vcpu.set_cpuid2(&cpuid).unwrap();
        let entries = boot_msrs();
        let msrs = Msrs::from_entries(&entries).unwrap();
        assert_eq!(vcpu.set_msrs(&msrs).unwrap(), entries.len());
        vcpu.set_regs(&boot_regs(ENTRY)).unwrap();
        vcpu.set_fpu(&boot_fpu()).unwrap();
        let (sregs, tables) = boot_sregs(vcpu.get_sregs().unwrap());
        for table in tables {
            memory
                .write_slice(&table.bytes, GuestAddress(table.address))
                .unwrap();
        }
        vcpu.set_sregs(&sregs).unwrap();
        vcpu.set_lapic(&configure_lint(vcpu.get_lapic().unwrap()))
            .unwrap();

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let device: Arc<dyn BusDevice> = Arc::new(BlockingReadDevice {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        });
        let pio = Arc::new(Bus::empty());
        let mmio = Arc::new(
            Bus::new(vec![(Range::from(MMIO_ADDRESS..MMIO_ADDRESS + 4), device)]).unwrap(),
        );
        let (event_tx, _event_rx) = tokio_mpsc::unbounded_channel();
        let handle = VcpuHandle::spawn(0, vcpu, pio, mmio, event_tx).unwrap();

        let (resume_reply, resumed) = oneshot::channel();
        handle.command(VcpuCommand::Resume(resume_reply)).unwrap();
        time::timeout(KVM_TEST_TIMEOUT, resumed)
            .await
            .expect("timed out waiting for resume acknowledgement")
            .expect("vCPU dropped resume acknowledgement");
        entered_rx
            .recv_timeout(KVM_TEST_TIMEOUT)
            .expect("timed out waiting for MMIO read");

        // Queue Pause while the read callback still owns KVM's pending MMIO data.
        let (pause_reply, paused) = oneshot::channel();
        handle.command(VcpuCommand::Pause(pause_reply)).unwrap();
        release_tx.send(()).unwrap();
        time::timeout(KVM_TEST_TIMEOUT, paused)
            .await
            .expect("timed out waiting for pause acknowledgement")
            .expect("vCPU dropped pause acknowledgement");

        let regs = handle.kick_fd.lock().unwrap().get_regs().unwrap();
        assert_eq!(regs.rax, u64::from(MMIO_READ_VALUE));
        assert_eq!(regs.rip, ENTRY + 5);
        assert_eq!(regs.rcx, 0, "instruction after the MMIO read executed");

        handle.command(VcpuCommand::Stop).unwrap();
        time::timeout(KVM_TEST_TIMEOUT, handle.wait())
            .await
            .expect("timed out waiting for vCPU stop")
            .unwrap();
        drop(vm);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn protected_mode_payload_uses_pvh_serial_and_i8042_on_kvm() {
        if !Path::new("/dev/kvm").exists() {
            return;
        }
        let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
        let vm = test_vm(&context);
        let memory = Arc::clone(vm.memory());

        const ENTRY: u64 = 0x10_0000;
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
        memory.write_obj(PVH_MAGIC, GuestAddress(0x6000)).unwrap();
        memory.write_slice(&code, GuestAddress(ENTRY)).unwrap();

        let vcpu = vm.create_vcpu(0).unwrap();
        let cpuid = normalize(
            &context.capabilities().supported_cpuid,
            &context.capabilities().host_cpuid,
            1,
            0,
        )
        .unwrap();
        vcpu.set_cpuid2(&cpuid).unwrap();
        let entries = boot_msrs();
        let msrs = Msrs::from_entries(&entries).unwrap();
        assert_eq!(vcpu.set_msrs(&msrs).unwrap(), entries.len());
        vcpu.set_regs(&boot_regs(ENTRY)).unwrap();
        vcpu.set_fpu(&boot_fpu()).unwrap();
        let (sregs, tables) = boot_sregs(vcpu.get_sregs().unwrap());
        for table in tables {
            memory
                .write_slice(&table.bytes, GuestAddress(table.address))
                .unwrap();
        }
        vcpu.set_sregs(&sregs).unwrap();
        vcpu.set_lapic(&configure_lint(vcpu.get_lapic().unwrap()))
            .unwrap();

        let output = Arc::new(Mutex::new(Vec::new()));
        let (input, _input_writer) = pipe();
        let (_serial_task, serial) =
            SerialDevice::new(input, SharedWriter(output.clone())).unwrap();
        vm.fd().register_irqfd(&serial.irq_fd, COM1_GSI.0).unwrap();
        let (event_tx, mut event_rx) = tokio_mpsc::unbounded_channel();
        let serial: Arc<dyn BusDevice> = Arc::new(serial);
        let i8042: Arc<dyn BusDevice> = Arc::new(I8042Device::new(event_tx.clone()));
        let pio = Arc::new(Bus::new(vec![(COM1_PORTS, serial), (I8042_PORTS, i8042)]).unwrap());
        let mmio = Arc::new(Bus::empty());
        let vcpus = Vcpus::new(vec![
            VcpuHandle::spawn(0, vcpu, pio, mmio, event_tx).unwrap(),
        ]);

        vcpus.pause().await.unwrap();
        vcpus.resume().await.unwrap();
        let event = time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, MachineEvent::GuestReset));
        assert_eq!(*output.lock().unwrap(), b"OK");
        vcpus.stop().await.unwrap();
        drop(vm);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn capture_state_returns_set_registers_on_kvm() {
        if !Path::new("/dev/kvm").exists() {
            return;
        }
        let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
        let vm = test_vm(&context);
        let memory = Arc::clone(vm.memory());

        const ENTRY: u64 = 0x10_0000;
        let vcpu = vm.create_vcpu(0).unwrap();
        let cpuid = normalize(
            &context.capabilities().supported_cpuid,
            &context.capabilities().host_cpuid,
            1,
            0,
        )
        .unwrap();
        vcpu.set_cpuid2(&cpuid).unwrap();
        let entries = boot_msrs();
        let msrs = Msrs::from_entries(&entries).unwrap();
        assert_eq!(vcpu.set_msrs(&msrs).unwrap(), entries.len());
        let set_regs = boot_regs(ENTRY);
        vcpu.set_regs(&set_regs).unwrap();
        let set_fpu = boot_fpu();
        vcpu.set_fpu(&set_fpu).unwrap();
        let (sregs, tables) = boot_sregs(vcpu.get_sregs().unwrap());
        for table in tables {
            memory
                .write_slice(&table.bytes, GuestAddress(table.address))
                .unwrap();
        }
        vcpu.set_sregs(&sregs).unwrap();
        vcpu.set_lapic(&configure_lint(vcpu.get_lapic().unwrap()))
            .unwrap();

        let pio = Arc::new(Bus::empty());
        let mmio = Arc::new(Bus::empty());
        let (event_tx, _event_rx) = tokio_mpsc::unbounded_channel();
        // vCPU starts paused, so capture_state can read immediately.
        let handle = VcpuHandle::spawn(0, vcpu, pio, mmio, event_tx).unwrap();

        let snapshot = handle.capture_state().unwrap();

        assert_eq!(snapshot.index, 0);
        assert_eq!(snapshot.regs.rip, set_regs.rip);
        assert_eq!(snapshot.regs.rsp, set_regs.rsp);
        assert_eq!(snapshot.regs.rflags, set_regs.rflags);
        // The FCW sits at the start of the legacy x87 region inside the XSAVE
        // area (lower 16 bits of region[0]).
        assert_eq!(
            snapshot.xsave.region[0] as u16, set_fpu.fcw,
            "FCW mismatch in XSAVE region"
        );
        assert!(!snapshot.cpuid.as_slice().is_empty(), "cpuid entries > 0");
        assert!(!snapshot.msrs.is_empty(), "msrs non-empty");

        handle.command(VcpuCommand::Stop).unwrap();
        time::timeout(KVM_TEST_TIMEOUT, handle.wait())
            .await
            .expect("timed out waiting for vCPU stop")
            .unwrap();
        drop(vm);
    }
}
