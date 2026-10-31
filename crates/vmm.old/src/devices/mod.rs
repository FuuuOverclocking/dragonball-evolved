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

/// Whether the device's I/O directly affects guest-visible state.
///
/// `GuestAffecting` tasks are paused by [`DeviceManager::quiesce`] and resumed by
/// [`DeviceManager::resume`]; `HostOnly` tasks keep running through a quiesce and
/// are only stopped by [`DeviceManager::stop`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceRole {
    /// Block I/O, serial input, IRQ resampling -- anything the guest observes.
    GuestAffecting,
    /// Console output drain -- visible only on the host side.
    HostOnly,
}

/// Control message sent through the per-device gate channel.
enum GateCommand {
    /// Pause the device future and acknowledge once quiesced.
    Quiesce(oneshot::Sender<()>),
    /// Resume the device future and acknowledge once running.
    Resume(oneshot::Sender<()>),
    /// Let the device future run to see the stop watch (set beforehand).
    Stop,
}

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

/// Wrap a `GuestAffecting` future so it can be paused and resumed.
///
/// In the *Running* state the supervisor biased-selects control messages against
/// the pinned inner future.  A `Quiesce` transitions to *Quiesced*, where only
/// control messages are processed and the inner future is never polled.  `Resume`
/// returns to *Running* with the same future and async stack intact.  `Stop` exits
/// the gate loop so the inner future runs alone (and sees the stop watch the
/// caller set just before sending `Stop`).
async fn gated<F>(mut gate: mpsc::UnboundedReceiver<GateCommand>, device: F) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    tokio::pin!(device);
    loop {
        tokio::select! {
            biased;
            cmd = gate.recv() => {
                match cmd {
                    Some(GateCommand::Quiesce(ack)) => {
                        let _ = ack.send(());
                        // ---- Quiesced: only process commands ----
                        loop {
                            match gate.recv().await {
                                Some(GateCommand::Resume(ack)) => {
                                    let _ = ack.send(());
                                    break; // back to Running
                                }
                                Some(GateCommand::Stop) => return device.await,
                                Some(GateCommand::Quiesce(ack)) => {
                                    let _ = ack.send(()); // idempotent
                                }
                                None => return device.await, // manager dropped
                            }
                        }
                    }
                    Some(GateCommand::Resume(ack)) => {
                        let _ = ack.send(()); // already running
                    }
                    Some(GateCommand::Stop) => return device.await,
                    None => return device.await,
                }
            }
            result = &mut device => return result,
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
    /// One sender per `GuestAffecting` task, used for quiesce / resume / stop.
    /// Dead senders (task already exited) are harmlessly skipped.
    guest_gates: Vec<mpsc::UnboundedSender<GateCommand>>,
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
            guest_gates: Vec::new(),
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
    ///
    /// `GuestAffecting` devices are wrapped in a gate supervisor so they can be
    /// paused and resumed without dropping the future.
    pub fn spawn<F>(&mut self, name: impl Into<String>, role: DeviceRole, device: F)
    where
        F: Future<Output = Result<()>> + 'static,
    {
        let name = name.into();
        match role {
            DeviceRole::GuestAffecting => {
                let (tx, rx) = mpsc::unbounded_channel();
                self.guest_gates.push(tx);
                spawn_supervised(name.clone(), self.exit_tx.clone(), gated(rx, device))
                    .expect("spawn device on local runtime");
            }
            DeviceRole::HostOnly => {
                spawn_supervised(name.clone(), self.exit_tx.clone(), device)
                    .expect("spawn device on local runtime");
            }
        }
        self.running += 1;
        info_unlimited!("starting device {name}");
    }

    /// Construct and spawn a device on the selected thread's local runtime.
    ///
    /// Only the factory crosses threads; its future may contain `!Send` state.
    /// Construction also runs inside the supervised task, so factory panics are
    /// device failures rather than failures of the I/O mailbox loop.
    ///
    /// `GuestAffecting` devices are wrapped in a gate supervisor; `HostOnly`
    /// devices receive only the stop watch.
    pub fn spawn_on<F, Fut>(
        &mut self,
        name: impl Into<String>,
        placement: Placement,
        role: DeviceRole,
        factory: F,
    ) -> Result<()>
    where
        F: FnOnce(Stop) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + 'static,
    {
        let name = name.into();
        let stop = self.stop_signal();
        let exit_tx = self.exit_tx.clone();
        let gate_rx = if role == DeviceRole::GuestAffecting {
            let (tx, rx) = mpsc::unbounded_channel();
            self.guest_gates.push(tx);
            Some(rx)
        } else {
            None
        };
        match placement {
            Placement::VmmMain => {
                spawn_supervised(name.clone(), exit_tx, async move {
                    let device = factory(stop);
                    match gate_rx {
                        Some(rx) => gated(rx, device).await,
                        None => device.await,
                    }
                })?;
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
                            let device = factory(stop);
                            match gate_rx {
                                Some(rx) => gated(rx, device).await,
                                None => device.await,
                            }
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

    /// Pause every `GuestAffecting` device without terminating it.
    ///
    /// Sends `Quiesce` to all connected gate channels and awaits their acks.
    /// A failed send or ack means the task already exited, which is fine.
    /// `HostOnly` tasks are unaffected.
    pub async fn quiesce(&mut self) {
        let mut acks = Vec::new();
        for gate in &self.guest_gates {
            let (tx, rx) = oneshot::channel();
            if gate.send(GateCommand::Quiesce(tx)).is_ok() {
                acks.push(rx);
            }
        }
        for ack in acks {
            let _ = ack.await;
        }
    }

    /// Resume every previously quiesced `GuestAffecting` device.
    ///
    /// Each task resumes its original future and async stack from where it was
    /// suspended.  If a task is already running the ack is sent immediately.
    pub async fn resume(&mut self) {
        let mut acks = Vec::new();
        for gate in &self.guest_gates {
            let (tx, rx) = oneshot::channel();
            if gate.send(GateCommand::Resume(tx)).is_ok() {
                acks.push(rx);
            }
        }
        for ack in acks {
            let _ = ack.await;
        }
    }

    /// Ask every device to stop, await every exit, then join the I/O thread.
    ///
    /// Works from both `Running` and `Quiesced` states and is idempotent.
    /// The stop watch is set first so every device future sees it once the gate
    /// lets it run; then `Stop` is sent to every guest gate to unpark any
    /// quiesced futures.
    pub async fn stop(&mut self) {
        // Unlike `send`, this stores the value even without any subscribers.
        self.stop_tx.send_replace(true);
        for gate in &self.guest_gates {
            let _ = gate.send(GateCommand::Stop);
        }
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
    use std::cell::Cell;
    use std::collections::HashSet;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread;
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;
    use crate::devices::async_fd::AsyncEventFd;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    /// Let tasks on this thread run without waiting for anything specific.
    async fn settle() {
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    // ---- existing tests, adapted to new DeviceRole / stop() API ----

    #[tokio::test(flavor = "local")]
    async fn local_spawn_accepts_non_send_future_and_preserves_errors() {
        let mut devices = DeviceManager::new();
        let state = Rc::new(42);
        devices.spawn("local", DeviceRole::GuestAffecting, async move {
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
        timeout(TEST_TIMEOUT, devices.stop()).await.unwrap();
    }

    #[tokio::test(flavor = "local")]
    async fn io_factory_constructs_non_send_future_on_target_runtime() {
        let mut devices = DeviceManager::new();
        let main_thread = thread::current().id();
        let (ready_tx, ready_rx) = oneshot::channel();
        devices
            .spawn_on("io", Placement::Io, DeviceRole::GuestAffecting, move |_| {
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
        timeout(TEST_TIMEOUT, devices.stop()).await.unwrap();
        assert!(devices.io_thread.is_none());
    }

    #[tokio::test(flavor = "local")]
    async fn stop_terminates_local_and_io_devices_and_joins_io_thread() {
        let mut devices = DeviceManager::new();
        let main_thread = thread::current().id();
        let (local_tx, local_rx) = oneshot::channel();
        let mut stop = devices.stop_signal();
        let state = Rc::new(main_thread);
        devices.spawn("legacy-local", DeviceRole::GuestAffecting, async move {
            stop.requested().await;
            local_tx.send(*state).unwrap();
            Ok(())
        });
        let mut completions = Vec::new();
        for placement in [Placement::VmmMain, Placement::Io] {
            let (done_tx, done_rx) = oneshot::channel();
            devices
                .spawn_on(
                    format!("{placement:?}"),
                    placement,
                    DeviceRole::GuestAffecting,
                    move |mut stop| {
                        let state = Rc::new(thread::current().id());
                        async move {
                            stop.requested().await;
                            done_tx.send(*state).unwrap();
                            Ok(())
                        }
                    },
                )
                .unwrap();
            completions.push((placement, done_rx));
        }
        assert_eq!(devices.running(), 3);
        timeout(TEST_TIMEOUT, devices.stop()).await.unwrap();
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
        timeout(TEST_TIMEOUT, devices.stop()).await.unwrap();
    }

    #[tokio::test(flavor = "local")]
    async fn future_and_factory_panics_report_exactly_one_exit() {
        let mut devices = DeviceManager::new();
        devices.spawn("legacy-panic", DeviceRole::GuestAffecting, async {
            panic!("legacy device panic")
        });
        for placement in [Placement::VmmMain, Placement::Io] {
            devices
                .spawn_on(
                    format!("future-{placement:?}"),
                    placement,
                    DeviceRole::GuestAffecting,
                    |_| async {
                        task::yield_now().await;
                        panic!("device future panic");
                    },
                )
                .unwrap();
            devices
                .spawn_on(
                    format!("factory-{placement:?}"),
                    placement,
                    DeviceRole::GuestAffecting,
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
        timeout(TEST_TIMEOUT, devices.stop()).await.unwrap();
        assert!(devices.io_thread.is_none());
    }

    #[tokio::test(flavor = "local")]
    async fn stop_does_not_hang_when_devices_panic() {
        let mut devices = DeviceManager::new();
        devices.spawn("legacy-panic", DeviceRole::GuestAffecting, async {
            panic!("legacy device panic")
        });
        for placement in [Placement::VmmMain, Placement::Io] {
            devices
                .spawn_on(
                    format!("panic-{placement:?}"),
                    placement,
                    DeviceRole::GuestAffecting,
                    |_| async {
                        panic!("device panic");
                    },
                )
                .unwrap();
        }
        timeout(TEST_TIMEOUT, devices.stop()).await.unwrap();
        assert_eq!(devices.running(), 0);
        assert!(devices.io_thread.is_none());
    }

    #[tokio::test(flavor = "local")]
    async fn stop_resolves_for_late_subscribers_and_sender_drop() {
        let mut devices = DeviceManager::new();
        devices.stop().await;
        let mut late_stop = devices.stop_signal();
        timeout(TEST_TIMEOUT, late_stop.requested()).await.unwrap();
        assert!(*late_stop.0.borrow());
        for placement in [Placement::VmmMain, Placement::Io] {
            devices
                .spawn_on(
                    format!("late-{placement:?}"),
                    placement,
                    DeviceRole::GuestAffecting,
                    |mut stop| async move {
                        stop.requested().await;
                        Ok(())
                    },
                )
                .unwrap();
        }
        timeout(TEST_TIMEOUT, devices.stop()).await.unwrap();
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
                .spawn_on(
                    "no-runtime",
                    Placement::VmmMain,
                    DeviceRole::GuestAffecting,
                    |_| async { Ok(()) }
                )
                .is_err()
        );
        assert_eq!(devices.running(), 0);
        assert!(devices.io_thread.is_none());
    }

    // ---- new lifecycle tests ----

    #[tokio::test(flavor = "local")]
    async fn quiesce_suspends_vmm_main_non_send_future_resume_continues() {
        let mut devices = DeviceManager::new();
        let counter = Rc::new(Cell::new(0u32));
        let c = Rc::clone(&counter);
        let (tick_tx, mut tick_rx) = mpsc::unbounded_channel::<()>();
        let mut stop = devices.stop_signal();

        devices.spawn("ticker", DeviceRole::GuestAffecting, async move {
            let _proof = Rc::new("!Send");
            loop {
                tokio::select! {
                    biased;
                    () = stop.requested() => return Ok(()),
                    _ = task::yield_now() => {
                        c.set(c.get() + 1);
                        let _ = tick_tx.send(());
                    }
                }
            }
        });

        // Wait for at least one tick (channel, not sleep).
        timeout(TEST_TIMEOUT, tick_rx.recv())
            .await
            .unwrap()
            .unwrap();

        // Quiesce.
        timeout(TEST_TIMEOUT, devices.quiesce()).await.unwrap();
        // Ticks queued before the quiesce must not count as progress after resume.
        while tick_rx.try_recv().is_ok() {}
        let at_quiesce = counter.get();

        // Settle to prove the future is no longer polled.
        settle().await;
        assert_eq!(
            counter.get(),
            at_quiesce,
            "VmmMain future advanced while quiesced"
        );

        // Resume.
        timeout(TEST_TIMEOUT, devices.resume()).await.unwrap();
        timeout(TEST_TIMEOUT, tick_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            counter.get() > at_quiesce,
            "VmmMain future did not advance after resume"
        );

        timeout(TEST_TIMEOUT, devices.stop()).await.unwrap();
    }

    #[tokio::test(flavor = "local")]
    async fn quiesce_suspends_io_non_send_future_resume_continues() {
        let mut devices = DeviceManager::new();
        let counter = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&counter);
        let (tick_tx, mut tick_rx) = mpsc::unbounded_channel::<()>();

        devices
            .spawn_on(
                "io-ticker",
                Placement::Io,
                DeviceRole::GuestAffecting,
                move |mut stop| {
                    let _proof = Rc::new("!Send on IO thread");
                    async move {
                        loop {
                            tokio::select! {
                                biased;
                                () = stop.requested() => return Ok(()),
                                _ = task::yield_now() => {
                                    c.fetch_add(1, Ordering::Relaxed);
                                    let _ = tick_tx.send(());
                                }
                            }
                        }
                    }
                },
            )
            .unwrap();

        timeout(TEST_TIMEOUT, tick_rx.recv())
            .await
            .unwrap()
            .unwrap();

        timeout(TEST_TIMEOUT, devices.quiesce()).await.unwrap();
        // Ticks queued before the quiesce must not count as progress after resume.
        while tick_rx.try_recv().is_ok() {}
        let at_quiesce = counter.load(Ordering::Relaxed);

        settle().await;
        assert_eq!(
            counter.load(Ordering::Relaxed),
            at_quiesce,
            "Io future advanced while quiesced"
        );

        timeout(TEST_TIMEOUT, devices.resume()).await.unwrap();
        timeout(TEST_TIMEOUT, tick_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            counter.load(Ordering::Relaxed) > at_quiesce,
            "Io future did not advance after resume"
        );

        timeout(TEST_TIMEOUT, devices.stop()).await.unwrap();
        assert!(devices.io_thread.is_none());
    }

    #[tokio::test(flavor = "local")]
    async fn host_only_device_continues_during_quiesce() {
        let mut devices = DeviceManager::new();
        let counter = Rc::new(Cell::new(0u32));
        let c = Rc::clone(&counter);
        let (tick_tx, mut tick_rx) = mpsc::unbounded_channel::<()>();
        let mut stop = devices.stop_signal();

        devices.spawn("host-only", DeviceRole::HostOnly, async move {
            loop {
                tokio::select! {
                    biased;
                    () = stop.requested() => return Ok(()),
                    _ = task::yield_now() => {
                        c.set(c.get() + 1);
                        let _ = tick_tx.send(());
                    }
                }
            }
        });

        timeout(TEST_TIMEOUT, tick_rx.recv())
            .await
            .unwrap()
            .unwrap();

        // Quiesce -- HostOnly should keep running.
        timeout(TEST_TIMEOUT, devices.quiesce()).await.unwrap();

        timeout(TEST_TIMEOUT, tick_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let after = counter.get();
        assert!(after > 0, "HostOnly device stopped during quiesce");

        timeout(TEST_TIMEOUT, devices.stop()).await.unwrap();
        assert_eq!(devices.running(), 0);
    }

    #[tokio::test(flavor = "local")]
    async fn stop_from_running_state_with_io_thread() {
        let mut devices = DeviceManager::new();
        for placement in [Placement::VmmMain, Placement::Io] {
            devices
                .spawn_on(
                    format!("{placement:?}"),
                    placement,
                    DeviceRole::GuestAffecting,
                    move |mut stop| async move {
                        stop.requested().await;
                        Ok(())
                    },
                )
                .unwrap();
        }
        assert_eq!(devices.running(), 2);
        timeout(TEST_TIMEOUT, devices.stop()).await.unwrap();
        assert_eq!(devices.running(), 0);
        assert!(devices.io_thread.is_none());
    }

    #[tokio::test(flavor = "local")]
    async fn stop_from_quiesced_state_with_io_thread() {
        let mut devices = DeviceManager::new();
        for placement in [Placement::VmmMain, Placement::Io] {
            devices
                .spawn_on(
                    format!("{placement:?}"),
                    placement,
                    DeviceRole::GuestAffecting,
                    move |mut stop| async move {
                        stop.requested().await;
                        Ok(())
                    },
                )
                .unwrap();
        }
        settle().await;
        timeout(TEST_TIMEOUT, devices.quiesce()).await.unwrap();
        timeout(TEST_TIMEOUT, devices.stop()).await.unwrap();
        assert_eq!(devices.running(), 0);
        assert!(devices.io_thread.is_none());
    }

    #[tokio::test(flavor = "local")]
    async fn quiesce_and_resume_are_idempotent() {
        let mut devices = DeviceManager::new();
        let (tick_tx, mut tick_rx) = mpsc::unbounded_channel::<()>();
        let mut stop = devices.stop_signal();

        devices.spawn("device", DeviceRole::GuestAffecting, async move {
            loop {
                tokio::select! {
                    biased;
                    () = stop.requested() => return Ok(()),
                    _ = task::yield_now() => { let _ = tick_tx.send(()); }
                }
            }
        });

        timeout(TEST_TIMEOUT, tick_rx.recv()).await.unwrap();

        // Double quiesce should not hang.
        timeout(TEST_TIMEOUT, devices.quiesce()).await.unwrap();
        timeout(TEST_TIMEOUT, devices.quiesce()).await.unwrap();
        while tick_rx.try_recv().is_ok() {}

        // Double resume should not hang.
        timeout(TEST_TIMEOUT, devices.resume()).await.unwrap();
        timeout(TEST_TIMEOUT, devices.resume()).await.unwrap();

        timeout(TEST_TIMEOUT, tick_rx.recv()).await.unwrap();
        timeout(TEST_TIMEOUT, devices.stop()).await.unwrap();
    }
}
