use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering, fence};
use std::thread;

use anyhow::{Context, Result, anyhow};
use kvm_bindings::kvm_regs;
use kvm_ioctls::{VcpuExit as KvmExit, VcpuFd};
use libc::{c_int, c_void, siginfo_t};
use tokio::sync::mpsc;
use vmm_sys_util::signal::{Killable, SIGRTMIN, register_signal_handler};

use crate::vm::Vm;

/// A terminal vCPU outcome delivered directly to the async main loop.
#[derive(Debug, PartialEq, Eq)]
pub enum VcpuExit {
    Completed,
    Shutdown,
    Stopped,
    Error(String),
}

pub struct Vcpu {
    fd: VcpuFd,
    exit_port: u16,
}

impl Vcpu {
    pub fn new(vm: &Vm, entry: u64, exit_port: u16) -> Result<(Self, VcpuControl)> {
        let fd = vm.create_vcpu(0)?;
        let mut sregs = fd.get_sregs().context("get vCPU special registers")?;
        sregs.cs.base = 0;
        sregs.cs.selector = 0;
        fd.set_sregs(&sregs)
            .context("set real-mode vCPU special registers")?;
        fd.set_regs(&kvm_regs {
            rip: entry,
            rflags: 2,
            ..Default::default()
        })
        .context("set vCPU registers")?;

        let control_fd = vm.duplicate_vcpu(&fd)?;
        let stopped = Arc::new(AtomicBool::new(false));
        Ok((
            Self { fd, exit_port },
            VcpuControl {
                fd: control_fd,
                stopped,
                finished: Arc::new(AtomicBool::new(false)),
                thread: None,
            },
        ))
    }

    pub fn start(
        self,
        mut control: VcpuControl,
        exits: mpsc::UnboundedSender<VcpuExit>,
    ) -> Result<VcpuHandle> {
        install_kick_handler()?;

        let stopped = Arc::clone(&control.stopped);
        let finished = Arc::clone(&control.finished);
        let thread = thread::Builder::new()
            .name("vcpu-0".into())
            .spawn(move || run(self.fd, self.exit_port, &stopped, &finished, exits))
            .context("spawn vCPU thread")?;
        control.thread = Some(thread);
        Ok(VcpuHandle { control })
    }
}

pub struct VcpuHandle {
    control: VcpuControl,
}

impl VcpuHandle {
    pub fn stop(&mut self) -> Result<()> {
        let Some(thread) = self.control.thread.take() else {
            return Ok(());
        };

        if !self.control.finished.load(Ordering::Acquire) {
            self.control.stopped.store(true, Ordering::Release);
            self.control.fd.set_kvm_immediate_exit(1);
            fence(Ordering::Release);
            thread
                .kill(SIGRTMIN())
                .map_err(|e| anyhow!("kick vCPU thread: {e}"))?;
        }

        thread.join().map_err(|_| anyhow!("vCPU thread panicked"))
    }
}

impl Drop for VcpuHandle {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub(crate) struct VcpuControl {
    fd: VcpuFd,
    stopped: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

fn run(
    mut fd: VcpuFd,
    exit_port: u16,
    stopped: &AtomicBool,
    finished: &AtomicBool,
    exits: mpsc::UnboundedSender<VcpuExit>,
) {
    let outcome = loop {
        if stopped.load(Ordering::Acquire) {
            break VcpuExit::Stopped;
        }

        match fd.run() {
            Ok(KvmExit::IoOut(port, data)) if port == exit_port && data.len() == 1 => {
                break VcpuExit::Completed;
            }
            Ok(KvmExit::Hlt) => break VcpuExit::Completed,
            Ok(KvmExit::Shutdown) => break VcpuExit::Shutdown,
            Ok(exit) => break VcpuExit::Error(format!("unhandled KVM exit: {exit:?}")),
            Err(e) if e.errno() == libc::EINTR => {
                fd.set_kvm_immediate_exit(0);
            }
            Err(e) => break VcpuExit::Error(format!("KVM_RUN failed: {e}")),
        }
    };

    finished.store(true, Ordering::Release);
    let _ = exits.send(outcome);
}

fn install_kick_handler() -> Result<()> {
    extern "C" fn kick(_: c_int, _: *mut siginfo_t, _: *mut c_void) {
        fence(Ordering::Acquire);
    }

    register_signal_handler(SIGRTMIN(), kick).map_err(|e| anyhow!("install vCPU kick signal: {e}"))
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::time::Duration;

    use vm_memory::{Bytes, GuestAddress};

    use super::*;
    use crate::memory::GuestRam;

    #[test]
    fn stop_interrupts_a_vcpu_inside_kvm_run() {
        if OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .is_err()
        {
            return;
        }

        const ENTRY: u64 = 0x1000;
        let ram = GuestRam::new(0x20_000).unwrap();
        ram.memory()
            .write_slice(&[0xeb, 0xfe], GuestAddress(ENTRY))
            .unwrap();
        let vm = Vm::new(&ram).unwrap();
        let (vcpu, control) = Vcpu::new(&vm, ENTRY, 0x1001).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut handle = vcpu.start(control, tx).unwrap();

        thread::sleep(Duration::from_millis(10));
        handle.stop().unwrap();
        assert_eq!(rx.blocking_recv(), Some(VcpuExit::Stopped));
    }
}
