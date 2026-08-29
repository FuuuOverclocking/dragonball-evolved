use std::os::fd::AsRawFd;

use anyhow::{Context, Result, bail};
use kvm_bindings::{KVM_MAX_CPUID_ENTRIES, kvm_pit_config};
use kvm_ioctls::{Cap, IoEventAddress, Kvm, NoDatamatch, VcpuFd, VmFd};
use vmm_sys_util::eventfd::EventFd;

use crate::memory::GuestRam;

const KVM_TSS_ADDRESS: usize = 0xfffb_d000;

/// The kernel-owned part of a single-vCPU x86 machine.
#[derive(Debug)]
pub struct Vm {
    kvm: Kvm,
    fd: VmFd,
}

impl Vm {
    pub fn new(ram: &GuestRam) -> Result<Self> {
        let kvm = Kvm::new().context("open /dev/kvm")?;
        for capability in [
            Cap::UserMemory,
            Cap::Irqchip,
            Cap::Pit2,
            Cap::Ioeventfd,
            Cap::Irqfd,
        ] {
            if !kvm.check_extension(capability) {
                bail!("KVM capability {capability:?} is unavailable");
            }
        }

        let fd = kvm.create_vm().context("create KVM VM")?;
        fd.set_tss_address(KVM_TSS_ADDRESS)
            .context("set KVM TSS address")?;
        fd.create_irq_chip().context("create in-kernel irqchip")?;
        fd.create_pit2(kvm_pit_config::default())
            .context("create in-kernel PIT")?;
        ram.register(&fd)?;

        Ok(Self { kvm, fd })
    }

    pub fn create_vcpu(&self, id: u64) -> Result<VcpuFd> {
        let vcpu = self.fd.create_vcpu(id).context("create KVM vCPU")?;
        let cpuid = self
            .kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .context("query supported CPUID")?;
        vcpu.set_cpuid2(&cpuid).context("set vCPU CPUID")?;
        Ok(vcpu)
    }

    pub fn register_pio_event(&self, event: &EventFd, port: u64) -> Result<()> {
        self.fd
            .register_ioevent(event, &IoEventAddress::Pio(port), NoDatamatch)
            .with_context(|| format!("register ioeventfd at PIO {port:#x}"))
    }

    pub fn register_irq(&self, event: &EventFd, gsi: u32) -> Result<()> {
        self.fd
            .register_irqfd(event, gsi)
            .with_context(|| format!("register irqfd for GSI {gsi}"))
    }

    /// Duplicate a vCPU fd so another thread can set `immediate_exit` on the same
    /// KVM vCPU mapping before sending its kick signal.
    pub(crate) fn duplicate_vcpu(&self, vcpu: &VcpuFd) -> Result<VcpuFd> {
        // SAFETY: `vcpu` is live and owned by the caller. `dup` creates an
        // independent descriptor, which `create_vcpu_from_rawfd` takes ownership
        // of on success.
        let fd = unsafe { libc::dup(vcpu.as_raw_fd()) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("duplicate vCPU fd");
        }

        // SAFETY: `fd` is the fresh duplicate above and no other owner will use it.
        // If wrapping fails, close it because ownership was not transferred.
        match unsafe { self.fd.create_vcpu_from_rawfd(fd) } {
            Ok(vcpu) => Ok(vcpu),
            Err(e) => {
                // SAFETY: wrapping failed, so this function still owns `fd`.
                unsafe { libc::close(fd) };
                Err(e).context("map duplicate vCPU fd")
            }
        }
    }
}
