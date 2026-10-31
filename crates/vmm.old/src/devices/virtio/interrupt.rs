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
/// The IRQ fd is written, while its paired resample fd reports kernel EOIs.
#[derive(Debug)]
pub struct IrqTrigger {
    status: Arc<AtomicU32>,
    irq_fd: EventFd,
    resample_fd: EventFd,
}

impl IrqTrigger {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            status: Arc::new(AtomicU32::new(0)),
            irq_fd: EventFd::new(EFD_NONBLOCK | EFD_CLOEXEC)?,
            resample_fd: EventFd::new(EFD_NONBLOCK | EFD_CLOEXEC)?,
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

    pub fn resample_fd(&self) -> &EventFd {
        &self.resample_fd
    }

    pub async fn resample(self: Arc<Self>, mut stop: crate::devices::Stop) -> anyhow::Result<()> {
        let resample =
            crate::devices::async_fd::AsyncEventFd::from_event_fd(self.resample_fd.try_clone()?)?;
        loop {
            tokio::select! {
                biased;
                () = stop.requested() => return Ok(()),
                result = resample.read() => { result?; }
            }
            if self.status.load(Ordering::SeqCst) != 0 {
                self.irq_fd.write(1)?;
            }
            tokio::task::yield_now().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::devices::async_fd::AsyncEventFd;
    use crate::devices::{DeviceManager, DeviceRole};

    #[tokio::test(flavor = "local")]
    async fn resampling_reasserts_pending_interrupts_but_not_acknowledged_ones() {
        let interrupt = Arc::new(IrqTrigger::new().unwrap());
        let irq = AsyncEventFd::from_event_fd(interrupt.irq_fd().try_clone().unwrap()).unwrap();
        let mut devices = DeviceManager::new();
        let stop = devices.stop_signal();
        devices.spawn(
            "resampler",
            DeviceRole::GuestAffecting,
            Arc::clone(&interrupt).resample(stop),
        );
        interrupt.trigger(InterruptKind::UsedRing).unwrap();
        irq.read().await.unwrap();
        interrupt.resample_fd().write(1).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), irq.read())
            .await
            .unwrap()
            .unwrap();
        interrupt.status.store(0, Ordering::SeqCst);
        interrupt.resample_fd().write(1).unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), irq.read())
                .await
                .is_err()
        );
        devices.stop().await;
    }

    #[tokio::test(flavor = "local")]
    async fn irq_resampler_quiesce_blocks_reinjection_resume_processes() {
        let interrupt = Arc::new(IrqTrigger::new().unwrap());
        let irq = AsyncEventFd::from_event_fd(interrupt.irq_fd().try_clone().unwrap()).unwrap();
        let mut devices = DeviceManager::new();
        let stop = devices.stop_signal();
        devices.spawn(
            "resampler",
            DeviceRole::GuestAffecting,
            Arc::clone(&interrupt).resample(stop),
        );

        // Trigger a used-ring interrupt, consume it.
        interrupt.trigger(InterruptKind::UsedRing).unwrap();
        tokio::time::timeout(Duration::from_secs(1), irq.read())
            .await
            .unwrap()
            .unwrap();

        // Quiesce the resampler.
        tokio::time::timeout(Duration::from_secs(1), devices.quiesce())
            .await
            .unwrap();

        // Simulate a guest EOI while quiesced.
        interrupt.resample_fd().write(1).unwrap();

        // The resampler must NOT re-inject while quiesced.
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(20), irq.read())
                .await
                .is_err(),
            "resampler re-injected while quiesced"
        );

        // Resume -- the saved resample event should be processed and re-inject.
        tokio::time::timeout(Duration::from_secs(1), devices.resume())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), irq.read())
            .await
            .expect("resampler did not re-inject after resume")
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), devices.stop())
            .await
            .unwrap();
    }
}
