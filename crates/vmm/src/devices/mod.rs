//! Device model.
//!
//! A device here is an ordinary async task pinned to a `LocalRuntime` on the
//! vmm thread or a dedicated I/O thread. It owns its descriptors, its emulation
//! state and its I/O backend outright, and it decides what to wait for next.
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
mod io_thread;
pub mod legacy;
pub mod rate_limiter;
pub mod virtio;

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};

use anyhow::{Result, anyhow};
use logger::{error_unlimited, info_unlimited};
use tokio::sync::{mpsc, watch};
use tokio::task;

use self::io_thread::IoThread;

/// The thread whose local runtime owns a device's emulation and I/O state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Placement {
    VmmMain,
    Io,
}

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

/// Start and monitor a local task, translating panics and cancellation into exits.
///
/// Only the monitor reports an exit, so every accepted device is counted once.
fn spawn_supervised<F>(
    name: String,
    exit_tx: mpsc::UnboundedSender<DeviceExit>,
    device: F,
) -> Result<()>
where
    F: Future<Output = Result<()>> + 'static,
{
    // `spawn_local` panics when the caller has no local runtime. Placement's
    // fallible API must report that without registering a device that never ran.
    let device = catch_unwind(AssertUnwindSafe(|| task::spawn_local(device)))
        .map_err(|_| anyhow!("device spawn requires an active local runtime"))?;
    task::spawn(async move {
        let result = match device.await {
            Ok(result) => result,
            Err(error) => Err(error.into()),
        };
        let _ = exit_tx.send(DeviceExit { name, result });
    });
    Ok(())
}

/// Owns device tasks on the vmm runtime and, lazily, an I/O runtime.
///
/// This is what is left of `event-manager`'s registry once devices drive
/// themselves: a way to ask them all to stop, and a way to hear that one exited.
#[derive(Debug)]
pub struct DeviceManager {
    stop_tx: watch::Sender<bool>,
    exit_tx: mpsc::UnboundedSender<DeviceExit>,
    exit_rx: mpsc::UnboundedReceiver<DeviceExit>,
    running: usize,
    io_thread: Option<IoThread>,
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
            io_thread: None,
        }
    }

    /// A stop signal to hand to a device before spawning it.
    pub fn stop_signal(&self) -> Stop {
        Stop(self.stop_tx.subscribe())
    }

    /// Spawn an already-constructed device on the current thread's runtime.
    ///
    /// The future never leaves this thread, which is what lets a device keep
    /// `!Send` state and skip locking its own emulation path.
    pub fn spawn<F>(&mut self, name: impl Into<String>, device: F)
    where
        F: Future<Output = Result<()>> + 'static,
    {
        let name = name.into();
        spawn_supervised(name.clone(), self.exit_tx.clone(), device)
            .expect("spawn device on local runtime");
        self.running += 1;
        info_unlimited!("starting device {name}");
    }

    /// Construct and spawn a device on the selected thread's local runtime.
    ///
    /// Only the factory crosses threads; its future may contain `!Send` state.
    /// Construction also runs inside the supervised task, so factory panics are
    /// device failures rather than failures of the I/O mailbox loop.
    pub fn spawn_on<F, Fut>(
        &mut self,
        name: impl Into<String>,
        placement: Placement,
        factory: F,
    ) -> Result<()>
    where
        F: FnOnce(Stop) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + 'static,
    {
        let name = name.into();
        let stop = self.stop_signal();
        let exit_tx = self.exit_tx.clone();
        match placement {
            Placement::VmmMain => {
                spawn_supervised(name.clone(), exit_tx, async move { factory(stop).await })?;
            }
            Placement::Io => {
                let io_thread = match self.io_thread.as_mut() {
                    Some(io_thread) => io_thread,
                    None => self.io_thread.insert(IoThread::new()?),
                };
                let device_name = name.clone();
                io_thread.spawn(Box::new(move || {
                    if let Err(error) =
                        spawn_supervised(device_name.clone(), exit_tx.clone(), async move {
                            factory(stop).await
                        })
                    {
                        let _ = exit_tx.send(DeviceExit {
                            name: device_name,
                            result: Err(error),
                        });
                    }
                }))?;
            }
        }
        self.running += 1;
        info_unlimited!("starting device {name}");
        Ok(())
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

    /// Ask every device to stop, await every exit, then join the I/O thread.
    pub async fn quiesce(&mut self) {
        // Unlike `send`, this stores the value even without any subscribers.
        self.stop_tx.send_replace(true);
        while self.running > 0 {
            let Some(exit) = self.next_exit().await else {
                break;
            };
            match exit.result {
                Ok(()) => info_unlimited!("device {} stopped", exit.name),
                Err(e) => error_unlimited!("device {} failed while stopping: {e:?}", exit.name),
            }
        }
        if let Some(io_thread) = self.io_thread.take()
            && let Err(error) = io_thread.shutdown().await
        {
            error_unlimited!("device I/O thread failed while stopping: {error:?}");
        }
    }
}

impl Default for DeviceManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::rc::Rc;
    use std::thread;
    use std::time::Duration;

    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::*;
    use crate::devices::async_fd::AsyncEventFd;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    #[tokio::test(flavor = "local")]
    async fn local_spawn_accepts_non_send_future_and_preserves_errors() {
        let mut devices = DeviceManager::new();
        let state = Rc::new(42);
        devices.spawn("local", async move {
            task::yield_now().await;
            assert_eq!(*state, 42);
            Err(anyhow!("device failure"))
        });
        assert_eq!(devices.running(), 1);
        let exit = timeout(TEST_TIMEOUT, devices.next_exit())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exit.name, "local");
        assert_eq!(exit.result.unwrap_err().to_string(), "device failure");
        assert_eq!(devices.running(), 0);
        assert!(devices.io_thread.is_none());
        timeout(TEST_TIMEOUT, devices.quiesce()).await.unwrap();
    }

    #[tokio::test(flavor = "local")]
    async fn io_factory_constructs_non_send_future_on_target_runtime() {
        let mut devices = DeviceManager::new();
        let main_thread = thread::current().id();
        let (ready_tx, ready_rx) = oneshot::channel();
        devices
            .spawn_on("io", Placement::Io, move |_| {
                // Both reactor registration and !Send state construction must
                // happen on the I/O runtime, not before crossing the mailbox.
                let state = Rc::new(thread::current().id());
                let event = AsyncEventFd::new().unwrap();
                ready_tx.send((*state, event.notifier().unwrap())).unwrap();
                async move {
                    assert_eq!(event.read().await?, 1);
                    assert_eq!(*state, thread::current().id());
                    Ok(())
                }
            })
            .unwrap();
        let (io_thread, notifier) = timeout(TEST_TIMEOUT, ready_rx).await.unwrap().unwrap();
        assert_ne!(main_thread, io_thread);
        notifier.write(1).unwrap();
        let exit = timeout(TEST_TIMEOUT, devices.next_exit())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exit.name, "io");
        exit.result.unwrap();
        assert_eq!(devices.running(), 0);
        timeout(TEST_TIMEOUT, devices.quiesce()).await.unwrap();
        assert!(devices.io_thread.is_none());
    }

    #[tokio::test(flavor = "local")]
    async fn quiesce_stops_local_and_io_devices_and_joins_io_thread() {
        let mut devices = DeviceManager::new();
        let main_thread = thread::current().id();
        let (local_tx, local_rx) = oneshot::channel();
        let mut stop = devices.stop_signal();
        let state = Rc::new(main_thread);
        devices.spawn("legacy-local", async move {
            stop.requested().await;
            local_tx.send(*state).unwrap();
            Ok(())
        });
        let mut completions = Vec::new();
        for placement in [Placement::VmmMain, Placement::Io] {
            let (done_tx, done_rx) = oneshot::channel();
            devices
                .spawn_on(format!("{placement:?}"), placement, move |mut stop| {
                    let state = Rc::new(thread::current().id());
                    async move {
                        stop.requested().await;
                        done_tx.send(*state).unwrap();
                        Ok(())
                    }
                })
                .unwrap();
            completions.push((placement, done_rx));
        }
        assert_eq!(devices.running(), 3);
        timeout(TEST_TIMEOUT, devices.quiesce()).await.unwrap();
        assert_eq!(devices.running(), 0);
        assert!(devices.io_thread.is_none());
        assert_eq!(local_rx.await.unwrap(), main_thread);
        for (placement, completion) in completions {
            let device_thread = completion.await.unwrap();
            match placement {
                Placement::VmmMain => assert_eq!(device_thread, main_thread),
                Placement::Io => assert_ne!(device_thread, main_thread),
            }
        }
        timeout(TEST_TIMEOUT, devices.quiesce()).await.unwrap();
    }

    #[tokio::test(flavor = "local")]
    async fn future_and_factory_panics_report_exactly_one_exit() {
        let mut devices = DeviceManager::new();
        devices.spawn("legacy-panic", async { panic!("legacy device panic") });
        for placement in [Placement::VmmMain, Placement::Io] {
            devices
                .spawn_on(format!("future-{placement:?}"), placement, |_| async {
                    task::yield_now().await;
                    panic!("device future panic");
                })
                .unwrap();
            devices
                .spawn_on(
                    format!("factory-{placement:?}"),
                    placement,
                    |_| -> std::future::Ready<Result<()>> { panic!("device factory panic") },
                )
                .unwrap();
        }
        let mut names = HashSet::new();
        for remaining in (0..5).rev() {
            let exit = timeout(TEST_TIMEOUT, devices.next_exit())
                .await
                .unwrap()
                .unwrap();
            assert!(names.insert(exit.name));
            let error = exit.result.unwrap_err();
            assert!(error.downcast_ref::<task::JoinError>().unwrap().is_panic());
            assert_eq!(devices.running(), remaining);
        }
        assert_eq!(names.len(), 5);
        assert!(matches!(
            devices.exit_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        timeout(TEST_TIMEOUT, devices.quiesce()).await.unwrap();
        assert!(devices.io_thread.is_none());
    }

    #[tokio::test(flavor = "local")]
    async fn quiesce_does_not_hang_when_devices_panic() {
        let mut devices = DeviceManager::new();
        devices.spawn("legacy-panic", async { panic!("legacy device panic") });
        for placement in [Placement::VmmMain, Placement::Io] {
            devices
                .spawn_on(format!("panic-{placement:?}"), placement, |_| async {
                    panic!("device panic");
                })
                .unwrap();
        }
        timeout(TEST_TIMEOUT, devices.quiesce()).await.unwrap();
        assert_eq!(devices.running(), 0);
        assert!(devices.io_thread.is_none());
    }

    #[tokio::test(flavor = "local")]
    async fn stop_resolves_for_late_subscribers_and_sender_drop() {
        let mut devices = DeviceManager::new();
        devices.quiesce().await;
        let mut late_stop = devices.stop_signal();
        timeout(TEST_TIMEOUT, late_stop.requested()).await.unwrap();
        assert!(*late_stop.0.borrow());
        for placement in [Placement::VmmMain, Placement::Io] {
            devices
                .spawn_on(
                    format!("late-{placement:?}"),
                    placement,
                    |mut stop| async move {
                        stop.requested().await;
                        Ok(())
                    },
                )
                .unwrap();
        }
        timeout(TEST_TIMEOUT, devices.quiesce()).await.unwrap();
        assert_eq!(devices.running(), 0);
        assert!(devices.io_thread.is_none());

        let devices = DeviceManager::new();
        let mut stop = devices.stop_signal();
        drop(devices);
        timeout(TEST_TIMEOUT, stop.requested()).await.unwrap();
    }

    #[test]
    fn main_placement_without_local_runtime_returns_error_without_counting() {
        let mut devices = DeviceManager::new();
        assert!(
            devices
                .spawn_on("no-runtime", Placement::VmmMain, |_| async { Ok(()) })
                .is_err()
        );
        assert_eq!(devices.running(), 0);
        assert!(devices.io_thread.is_none());
    }
}
