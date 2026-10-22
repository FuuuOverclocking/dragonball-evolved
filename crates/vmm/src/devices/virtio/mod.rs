//! virtio device support.

pub mod block;
pub mod interrupt;
pub mod mmio;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::sync::{mpsc, watch};
use virtio_queue::Queue;

pub use crate::devices::virtio::interrupt::{InterruptKind, IrqTrigger};

/// Guest physical memory as devices see it.
pub type GuestMemoryMmap = vm_memory::GuestMemoryMmap<()>;

/// A cheaply clonable handle to guest memory.
///
/// `Arc` is enough while the memory map is fixed at boot; growing the guest at
/// runtime would want `vm_memory::GuestMemoryAtomic` instead.
pub type SharedGuestMemory = Arc<GuestMemoryMmap>;

/// Everything the guest driver negotiates before a device may touch its queues.
#[derive(Debug)]
pub struct Activation {
    pub mem: SharedGuestMemory,
    pub queues: Vec<Queue>,
    pub interrupt: Arc<IrqTrigger>,
    pub driver_features: u64,
    pub session: Session,
}

#[derive(Debug)]
pub struct Session {
    reset: watch::Receiver<bool>,
    finished: Option<oneshot::Sender<()>>,
}

#[derive(Debug)]
pub struct SessionControl {
    reset: watch::Sender<bool>,
    finished: oneshot::Receiver<()>,
}

pub fn session_channel() -> (SessionControl, Session) {
    let (reset_tx, reset_rx) = watch::channel(false);
    let (finished_tx, finished_rx) = oneshot::channel();
    (
        SessionControl {
            reset: reset_tx,
            finished: finished_rx,
        },
        Session {
            reset: reset_rx,
            finished: Some(finished_tx),
        },
    )
}

impl Session {
    pub async fn requested(&mut self) {
        loop {
            if *self.reset.borrow_and_update() {
                return;
            }
            if self.reset.changed().await.is_err() {
                return;
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(finished) = self.finished.take() {
            let _ = finished.send(());
        }
    }
}

impl SessionControl {
    pub fn reset(&self) -> Result<()> {
        self.reset.send_replace(true);
        self.finished.recv_timeout(Duration::from_secs(5))?;
        Ok(())
    }
}

/// The sending half of the activation handshake, held by the mmio or pci
/// transport.
///
/// The transport runs on a vcpu thread, because the guest's `DRIVER_OK` write is
/// a synchronous mmio exit, so activation genuinely crosses a thread boundary.
/// Firecracker bridges it with a dedicated `activate_evt` eventfd -- not because
/// the handshake needs a descriptor, but because a subscriber can only re-register
/// its own descriptors from inside `process()`, and an inactive device must
/// somehow get itself called. With registrations no longer a shared resource that
/// reason disappears, so this is a plain channel and the device awaits it.
#[derive(Clone, Debug)]
pub struct Activator {
    tx: mpsc::Sender<Activation>,
}

impl Activator {
    /// Hand the negotiated queue configuration to the device task.
    ///
    /// Safe to call from a vcpu thread; it neither blocks nor allocates a
    /// descriptor.
    pub fn activate(&self, activation: Activation) -> Result<()> {
        if self.tx.try_send(activation).is_err() {
            bail!("device task is no longer running");
        }
        Ok(())
    }
}

/// The receiving half, owned by the device task.
#[derive(Debug)]
pub struct ActivationRx {
    rx: mpsc::Receiver<Activation>,
}

impl ActivationRx {
    /// Wait for the guest driver to bring the device up.
    ///
    /// `None` means the transport went away before the driver ever did, which is
    /// an ordinary end for a device the guest never used.
    pub async fn recv(&mut self) -> Option<Activation> {
        self.rx.recv().await
    }
}

/// Create the two halves of an activation handshake.
pub fn activation_channel() -> (Activator, ActivationRx) {
    let (tx, rx) = mpsc::channel(1);
    (Activator { tx }, ActivationRx { rx })
}
