//! Device model.
//!
//! A device here is an ordinary async task pinned to the vmm thread's
//! `LocalRuntime`. It owns its descriptors, its emulation state and its I/O
//! backend outright, and it decides for itself what to wait for next.
//!
//! What that removes, compared to driving devices from `event-manager`:
//!
//! * **The subscriber registry.** There is no `fd -> SubscriberId` map and no
//!   `Arc<Mutex<dyn MutEventSubscriber>>`. A device's emulation state has a
//!   single owner, so its hot path takes no locks.
//! * **The `u32` dispatch tags.** `PROCESS_QUEUE`, `PROCESS_RATE_LIMITER` and
//!   friends only exist because a subscriber is handed one opaque event and has
//!   to re-derive which of its descriptors it came from. Awaiting a descriptor
//!   where its readiness matters makes the tag redundant.
//! * **Registration churn as program state.** Firecracker encodes "inactive",
//!   "throttled" and "receive FIFO full" by adding and removing epoll
//!   registrations from inside `process()`. Those are just points at which a task
//!   is suspended.
//!
//! Because the tasks are local, a device may hold `!Send` state. The parts a vcpu
//! thread genuinely shares -- config space, queue configuration, the interrupt
//! line -- stay behind their own handles, which keeps that boundary explicit
//! instead of putting the whole device behind one mutex.

pub mod async_fd;
pub mod legacy;
pub mod rate_limiter;
pub mod virtio;

use std::future::Future;

use anyhow::Result;
use logger::{error_unrestricted, info_unrestricted};
use tokio::sync::{mpsc, watch};
use tokio::task;

/// A cooperative stop signal handed to every device task.
///
/// Devices quiesce at a point of their own choosing rather than being aborted,
/// so an in-flight request is never torn in half.
#[derive(Clone, Debug)]
pub struct Stop(watch::Receiver<bool>);

impl Stop {
    /// Resolve once the vmm has asked devices to stop.
    pub async fn requested(&mut self) {
        loop {
            let stopped = *self.0.borrow_and_update();
            if stopped {
                return;
            }
            // The sender being dropped means the vmm is going away too.
            if self.0.changed().await.is_err() {
                return;
            }
        }
    }
}

/// How a device task ended.
#[derive(Debug)]
pub struct DeviceExit {
    pub name: String,
    pub result: Result<()>,
}

/// Owns the device tasks running on the vmm thread.
///
/// This is what is left of `event-manager`'s registry once devices drive
/// themselves: a way to ask them all to stop, and a way to hear that one exited.
#[derive(Debug)]
pub struct DeviceManager {
    stop_tx: watch::Sender<bool>,
    exit_tx: mpsc::UnboundedSender<DeviceExit>,
    exit_rx: mpsc::UnboundedReceiver<DeviceExit>,
    running: usize,
}

impl DeviceManager {
    pub fn new() -> Self {
        let (stop_tx, _) = watch::channel(false);
        let (exit_tx, exit_rx) = mpsc::unbounded_channel();
        Self {
            stop_tx,
            exit_tx,
            exit_rx,
            running: 0,
        }
    }

    /// A stop signal to hand to a device before spawning it.
    pub fn stop_signal(&self) -> Stop {
        Stop(self.stop_tx.subscribe())
    }

    /// Spawn a device on the current thread's runtime.
    ///
    /// The future never leaves this thread, which is what lets a device keep
    /// `!Send` state and skip locking its own emulation path.
    pub fn spawn<F>(&mut self, name: impl Into<String>, device: F)
    where
        F: Future<Output = Result<()>> + 'static,
    {
        let name = name.into();
        let exit_tx = self.exit_tx.clone();
        self.running += 1;
        info_unrestricted!("starting device {name}");
        task::spawn_local(async move {
            let result = device.await;
            let _ = exit_tx.send(DeviceExit { name, result });
        });
    }

    /// Device tasks that have not reported an exit yet.
    pub fn running(&self) -> usize {
        self.running
    }

    /// Wait for the next device task to finish.
    ///
    /// A device exiting on its own is a vmm-level event, so the main loop selects
    /// on this the same way it selects on a vcpu exit. With no devices running
    /// this never resolves, which is the behaviour a `select!` arm wants.
    pub async fn next_exit(&mut self) -> Option<DeviceExit> {
        let exit = self.exit_rx.recv().await?;
        self.running -= 1;
        Some(exit)
    }

    /// Ask every device to stop and wait until all of them have.
    pub async fn quiesce(&mut self) {
        let _ = self.stop_tx.send(true);
        while self.running > 0 {
            let Some(exit) = self.next_exit().await else {
                break;
            };
            match exit.result {
                Ok(()) => info_unrestricted!("device {} stopped", exit.name),
                Err(e) => error_unrestricted!("device {} failed while stopping: {e:?}", exit.name),
            }
        }
    }
}

impl Default for DeviceManager {
    fn default() -> Self {
        Self::new()
    }
}
