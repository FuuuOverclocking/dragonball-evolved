//! virtio device support.

pub mod block;
pub mod interrupt;

use std::sync::Arc;

use anyhow::{Result, bail};
use tokio::sync::mpsc;
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
    tx: mpsc::UnboundedSender<Activation>,
}

impl Activator {
    /// Hand the negotiated queue configuration to the device task.
    ///
    /// Safe to call from a vcpu thread; it neither blocks nor allocates a
    /// descriptor.
    pub fn activate(&self, activation: Activation) -> Result<()> {
        if self.tx.send(activation).is_err() {
            bail!("device task is no longer running");
        }
        Ok(())
    }
}

/// The receiving half, owned by the device task.
#[derive(Debug)]
pub struct ActivationRx {
    rx: mpsc::UnboundedReceiver<Activation>,
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
    let (tx, rx) = mpsc::unbounded_channel();
    (Activator { tx }, ActivationRx { rx })
}
