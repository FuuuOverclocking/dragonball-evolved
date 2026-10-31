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
                    if self.status() != 0 {
                        self.irq_fd.write(1).map_err(ResampleError::Inject)?;
                    }
                }
            }
            tokio::task::yield_now().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::devices::runtime::{DeviceRuntime, Response, Start};

    const WITHIN: Duration = Duration::from_secs(1);

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
