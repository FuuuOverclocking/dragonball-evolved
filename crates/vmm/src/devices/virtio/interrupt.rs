use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use vmm_sys_util::eventfd::{EFD_CLOEXEC, EFD_NONBLOCK, EventFd};

use crate::devices::async_fd::AsyncEventFd;
use crate::devices::control::{Control, Flow};

pub const VIRTIO_MMIO_INT_VRING: u32 = 1;
pub const VIRTIO_MMIO_INT_CONFIG: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptKind {
    UsedRing,
    Config,
}

#[derive(Debug)]
pub struct IrqTrigger {
    status: AtomicU32,
    irq_fd: EventFd,
    resample_fd: EventFd,
}

/// invalid saved virtio interrupt status {0:#x}
#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub struct InterruptStatusError(pub u32);

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum ResampleError {
    /// register the interrupt resample descriptor
    Register(#[source] io::Error),
    /// read the interrupt resample descriptor
    Read(#[source] io::Error),
    /// reinject the pending interrupt
    Inject(#[source] io::Error),
}

impl IrqTrigger {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            status: AtomicU32::new(0),
            irq_fd: EventFd::new(EFD_NONBLOCK | EFD_CLOEXEC)?,
            resample_fd: EventFd::new(EFD_NONBLOCK | EFD_CLOEXEC)?,
        })
    }

    pub fn trigger(&self, kind: InterruptKind) -> io::Result<()> {
        let bit = match kind {
            InterruptKind::UsedRing => VIRTIO_MMIO_INT_VRING,
            InterruptKind::Config => VIRTIO_MMIO_INT_CONFIG,
        };
        self.status.fetch_or(bit, Ordering::SeqCst);
        self.irq_fd.write(1)
    }

    pub fn status(&self) -> u32 {
        self.status.load(Ordering::SeqCst)
    }

    pub fn acknowledge(&self, bits: u32) {
        self.status.fetch_and(!bits, Ordering::SeqCst);
    }

    /// Restore status without injecting an interrupt; the caller keeps workers quiesced.
    pub fn restore_status(&self, status: u32) -> Result<(), InterruptStatusError> {
        if status & !(VIRTIO_MMIO_INT_VRING | VIRTIO_MMIO_INT_CONFIG) != 0 {
            return Err(InterruptStatusError(status));
        }
        self.status.store(status, Ordering::SeqCst);
        Ok(())
    }

    pub fn reraise_pending(&self) -> io::Result<()> {
        if self.status() != 0 {
            self.irq_fd.write(1)?;
        }
        Ok(())
    }

    pub fn irq_fd(&self) -> &EventFd {
        &self.irq_fd
    }

    pub fn resample_fd(&self) -> &EventFd {
        &self.resample_fd
    }

    pub async fn resample(self: Arc<Self>, mut control: Control<()>) -> Result<(), ResampleError> {
        let fd = self
            .resample_fd
            .try_clone()
            .map_err(ResampleError::Register)?;
        let resample = AsyncEventFd::from_event_fd(fd).map_err(ResampleError::Register)?;
        loop {
            tokio::select! {
                biased;
                command = control.recv() => {
                    if control.handle(command, || ()).await == Flow::Stop {
                        return Ok(());
                    }
                }
                result = resample.read() => {
                    result.map_err(ResampleError::Read)?;
                    self.reraise_pending().map_err(ResampleError::Inject)?;
                }
            }
            tokio::task::yield_now().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;
    use std::time::Duration;

    use super::*;
    use crate::devices::runtime::{DeviceRuntime, Response, Start};
    use crate::error::render_chain;

    const WITHIN: Duration = Duration::from_secs(1);

    #[test]
    fn resample_errors_preserve_typed_sources() {
        for (error, message) in [
            (
                ResampleError::Register(io::Error::other("resample failure")),
                "register the interrupt resample descriptor",
            ),
            (
                ResampleError::Read(io::Error::other("resample failure")),
                "read the interrupt resample descriptor",
            ),
            (
                ResampleError::Inject(io::Error::other("resample failure")),
                "reinject the pending interrupt",
            ),
        ] {
            let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
            assert_eq!(source.kind(), io::ErrorKind::Other);
            assert_eq!(error.to_string(), message);
            assert_eq!(render_chain(&error), format!("{message}: resample failure"));
        }
    }

    #[test]
    fn restored_status_replaces_bits_without_injection() {
        let interrupt = IrqTrigger::new().unwrap();
        interrupt.restore_status(3).unwrap();
        interrupt.restore_status(1).unwrap();
        assert_eq!(interrupt.status(), 1);
        assert_eq!(interrupt.restore_status(5), Err(InterruptStatusError(5)));
        assert_eq!(interrupt.status(), 1);
        assert_eq!(
            interrupt.irq_fd().read().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        interrupt.restore_status(0).unwrap();
        interrupt.reraise_pending().unwrap();
        assert_eq!(interrupt.status(), 0);
        assert_eq!(
            interrupt.irq_fd().read().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[tokio::test(flavor = "local")]
    async fn resample_only_reinjects_pending_bits_and_parks() {
        let interrupt = Arc::new(IrqTrigger::new().unwrap());
        let irq = AsyncEventFd::from_event_fd(interrupt.irq_fd().try_clone().unwrap()).unwrap();
        let (mut runtime, _exits) = DeviceRuntime::new();
        let worker = Arc::clone(&interrupt);
        let handle = runtime
            .spawn("resample", Start::Running, move |control| {
                worker.resample(control)
            })
            .unwrap();
        interrupt.trigger(InterruptKind::UsedRing).unwrap();
        interrupt.trigger(InterruptKind::Config).unwrap();
        assert_eq!(interrupt.status(), 3);
        irq.read().await.unwrap();
        interrupt.acknowledge(1);
        assert_eq!(interrupt.status(), 2);
        interrupt.resample_fd().write(1).unwrap();
        tokio::time::timeout(WITHIN, irq.read())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(handle.quiesce(WITHIN).await, Response::Replied(()));
        interrupt.resample_fd().write(1).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), irq.read())
                .await
                .is_err()
        );
        assert_eq!(handle.resume(WITHIN).await, Response::Replied(()));
        tokio::time::timeout(WITHIN, irq.read())
            .await
            .unwrap()
            .unwrap();
        interrupt.acknowledge(2);
        interrupt.resample_fd().write(1).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), irq.read())
                .await
                .is_err()
        );
        runtime.shutdown(WITHIN).await.unwrap();
    }
}
