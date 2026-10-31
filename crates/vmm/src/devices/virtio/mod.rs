pub mod block;
pub mod interrupt;
pub mod mmio;

use std::sync::Arc;
use std::time::Duration;

pub use interrupt::{InterruptKind, IrqTrigger};
use tokio::sync::{mpsc, watch};
use virtio_queue::Queue;

use crate::memory::store::MemoryView;

#[derive(Debug)]
pub struct Activation {
    pub queues: Vec<Queue>,
    pub mem: MemoryView,
    pub interrupt: Arc<IrqTrigger>,
    pub driver_features: u64,
    // Session is last: queued activations release queues and RAM before acknowledging.
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
    acknowledged: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum ResetError {
    /// device session did not reset in time
    TimedOut,
    /// device session ended without acknowledging reset
    Exited,
}

pub fn session_channel() -> (SessionControl, Session) {
    let (reset_tx, reset_rx) = watch::channel(false);
    let (finished_tx, finished_rx) = oneshot::channel();
    (
        SessionControl {
            reset: reset_tx,
            finished: finished_rx,
            acknowledged: false,
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
    pub fn reset(&mut self, within: Duration) -> Result<(), ResetError> {
        if self.acknowledged {
            return Ok(());
        }
        self.reset.send_replace(true);
        match self
            .finished
            .recv_timeout(within.min(Duration::from_secs(5)))
        {
            Ok(()) => {
                self.acknowledged = true;
                Ok(())
            }
            Err(oneshot::RecvTimeoutError::Timeout) => Err(ResetError::TimedOut),
            Err(oneshot::RecvTimeoutError::Disconnected) => Err(ResetError::Exited),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum ActivationError {
    /// device activation is already queued
    Full,
    /// device activation receiver has exited
    Closed,
}

#[derive(Clone, Debug)]
pub struct Activator {
    tx: mpsc::Sender<Activation>,
}

impl Activator {
    pub fn activate(&self, activation: Activation) -> Result<(), ActivationError> {
        self.tx.try_send(activation).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => ActivationError::Full,
            mpsc::error::TrySendError::Closed(_) => ActivationError::Closed,
        })
    }
}

#[derive(Debug)]
pub struct ActivationRx {
    rx: mpsc::Receiver<Activation>,
}

impl ActivationRx {
    pub async fn recv(&mut self) -> Option<Activation> {
        self.rx.recv().await
    }

    pub fn try_recv(&mut self) -> Option<Activation> {
        self.rx.try_recv().ok()
    }
}

pub fn activation_channel() -> (Activator, ActivationRx) {
    let (tx, rx) = mpsc::channel(1);
    (Activator { tx }, ActivationRx { rx })
}
