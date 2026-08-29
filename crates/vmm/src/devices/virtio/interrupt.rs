//! Guest interrupt injection.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use vmm_sys_util::eventfd::{EFD_CLOEXEC, EFD_NONBLOCK, EventFd};

/// virtio-mmio interrupt status bits, as read back by the guest from
/// `VIRTIO_MMIO_INTERRUPT_STATUS`.
pub const VIRTIO_MMIO_INT_VRING: u32 = 0x01;
pub const VIRTIO_MMIO_INT_CONFIG: u32 = 0x02;

/// Why a device is interrupting the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptKind {
    /// The device added entries to a used ring.
    UsedRing,
    /// The device's configuration space changed.
    Config,
}

/// An `irqfd`: writing it makes KVM inject the device's interrupt.
///
/// This one is never awaited. Userspace only ever writes it, so it stays outside
/// the reactor for the same reason it stays outside firecracker's epoll set. It
/// is `Send + Sync` because both the device task and vcpu threads raise
/// interrupts.
#[derive(Debug)]
pub struct IrqTrigger {
    status: Arc<AtomicU32>,
    irq_fd: EventFd,
}

impl IrqTrigger {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            status: Arc::new(AtomicU32::new(0)),
            irq_fd: EventFd::new(EFD_NONBLOCK | EFD_CLOEXEC)?,
        })
    }

    /// Raise the interrupt line.
    pub fn trigger(&self, kind: InterruptKind) -> io::Result<()> {
        let bit = match kind {
            InterruptKind::UsedRing => VIRTIO_MMIO_INT_VRING,
            InterruptKind::Config => VIRTIO_MMIO_INT_CONFIG,
        };
        self.status.fetch_or(bit, Ordering::SeqCst);
        self.irq_fd.write(1)
    }

    /// The pending-interrupt bits the guest reads and acknowledges.
    pub fn status(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.status)
    }

    /// The descriptor registered with `KVM_IRQFD`.
    pub fn irq_fd(&self) -> &EventFd {
        &self.irq_fd
    }
}
