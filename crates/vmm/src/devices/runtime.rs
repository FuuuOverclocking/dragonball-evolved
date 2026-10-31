//! Where device tasks run, how their holders control them, and how their
//! exits are reported.
//!
//! [`DeviceRuntime`] only places tasks, supervises them and owns the I/O
//! thread. It owns no device: whoever attaches a device holds its
//! [`DeviceHandle`], and the device's state lives in its task.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use logger::{error_unlimited, info_unlimited};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task;
use tokio::time::{self, Instant};

use super::control::{Ack, Command, Control};
use super::io_thread::IoThread;
pub use super::io_thread::IoThreadError;
use crate::error::render_chain;

pub type BoxError = Box<dyn Error + Send + Sync>;

/// The thread whose local runtime runs a device task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// The thread that owns the [`DeviceRuntime`].
    VmmMain,
    /// The runtime's I/O thread, started on first use.
    Io,
}

/// How a device task begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    Running,
    /// Quiesced: the task does nothing guest-visible until its first `Resume`.
    Parked,
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum SpawnError {
    /// spawning a device task requires a local runtime on this thread
    NoLocalRuntime,
    /// place the device task on the I/O thread
    IoThread(#[from] IoThreadError),
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum ShutdownError {
    /// device tasks {0:?} did not stop in time
    Unresponsive(Vec<Arc<str>>),
    /// device I/O thread did not join in time
    IoJoinTimeout(#[source] time::error::Elapsed),
    /// stop the device I/O thread
    IoThread(#[source] IoThreadError),
}

/// Why a device task ended other than by returning `Ok`.
#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum DeviceFailure {
    /// device task failed
    Failed(#[source] BoxError),
    /// device task panicked
    Panicked(#[source] task::JoinError),
    /// device task was dropped before it finished
    Cancelled,
}

/// A device task that ended without having been asked to stop.
#[derive(Debug)]
pub struct DeviceExit {
    pub name: Arc<str>,
    pub result: Result<(), DeviceFailure>,
}

/// The stream of [`DeviceExit`]s; each task reports at most once.
#[derive(Debug)]
pub struct DeviceExits(mpsc::UnboundedReceiver<DeviceExit>);

impl DeviceExits {
    /// The next exit; `None` once the runtime and every task are gone.
    /// Cancel-safe.
    pub async fn next(&mut self) -> Option<DeviceExit> {
        self.0.recv().await
    }
}

/// What a device task did with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Response<T> {
    Replied(T),
    /// The task has ended, so there is nothing to quiesce or resume.
    Exited,
    /// The task did not answer in time; it may still answer later.
    TimedOut,
}

/// Resolves once a task has ended or been dropped: the sender lives exactly as
/// long as the task's supervisor.
#[derive(Debug, Clone)]
struct Finished(watch::Receiver<()>);

impl Finished {
    async fn wait(&mut self) {
        while self.0.changed().await.is_ok() {}
    }

    fn is_done(&self) -> bool {
        self.0.has_changed().is_err()
    }
}

/// The holder's side of one device task, generic over what the device reports
/// when quiesced. Dropping it stops the task.
#[derive(Debug)]
pub struct DeviceHandle<Q> {
    name: Arc<str>,
    commands: mpsc::UnboundedSender<Command<Q>>,
    stop_requested: Arc<AtomicBool>,
    finished: Finished,
}

impl<Q> DeviceHandle<Q> {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Asks the task to park and report its quiesced state, waiting at most
    /// `within` for the answer.
    pub async fn quiesce(&self, within: Duration) -> Response<Q> {
        self.request(Command::Quiesce, within).await
    }

    /// Asks a parked task to carry on, waiting at most `within` for the
    /// acknowledgement. A running task acknowledges at once.
    pub async fn resume(&self, within: Duration) -> Response<()> {
        self.request(Command::Resume, within).await
    }

    /// Asks the task to stop; [`Self::finished`] tells when it has.
    pub fn stop(&self) {
        self.stop_requested.store(true, Ordering::SeqCst);
        let _ = self.commands.send(Command::Stop);
    }

    /// Resolves once the task has ended.
    pub async fn finished(&self) {
        self.finished.clone().wait().await;
    }

    pub fn has_exited(&self) -> bool {
        self.finished.is_done()
    }

    async fn request<T>(&self, command: fn(Ack<T>) -> Command<Q>, within: Duration) -> Response<T> {
        let (reply, answer) = oneshot::channel();
        if self.commands.send(command(Ack::new(reply))).is_err() {
            return Response::Exited;
        }
        match time::timeout(within, answer).await {
            Ok(Ok(value)) => Response::Replied(value),
            // The task dropped the request together with its control.
            Ok(Err(_)) => Response::Exited,
            Err(_) => Response::TimedOut,
        }
    }
}

impl<Q> Drop for DeviceHandle<Q> {
    fn drop(&mut self) {
        // Closing the channel is the stop request itself.
        self.stop_requested.store(true, Ordering::SeqCst);
    }
}

/// Reports one task's end; dropping it marks the task finished.
struct Report {
    name: Arc<str>,
    stop_requested: Arc<AtomicBool>,
    exits: mpsc::UnboundedSender<DeviceExit>,
    _finished: watch::Sender<()>,
}

impl Report {
    fn exit(self, result: Result<(), DeviceFailure>) {
        let Self {
            name,
            stop_requested,
            exits,
            _finished: finished,
        } = self;
        // Finished before reported, so whoever reads the exit sees the task ended.
        drop(finished);
        let expected = stop_requested.load(Ordering::SeqCst);
        match (&result, expected) {
            (Ok(()), true) => info_unlimited!("device {name} stopped"),
            (Ok(()), false) => info_unlimited!("device {name} exited"),
            (Err(error), true) => {
                error_unlimited!(
                    "device {name} failed while stopping: {}",
                    render_chain(error)
                );
            }
            (Err(error), false) => {
                error_unlimited!("device {name} exited: {}", render_chain(error))
            }
        }
        if !expected {
            let _ = exits.send(DeviceExit { name, result });
        }
    }
}

/// Spawns `device` on the current thread's local runtime, with a supervisor
/// that turns its end, including a panic, into exactly one report.
fn spawn_supervised<F>(report: Report, device: F) -> Result<(), Report>
where
    F: Future<Output = Result<(), BoxError>> + 'static,
{
    if tokio::runtime::Handle::try_current().is_err() {
        return Err(report);
    }
    // `spawn_local` panics on a runtime that is not local.
    let Ok(task) = catch_unwind(AssertUnwindSafe(|| task::spawn_local(device))) else {
        return Err(report);
    };
    task::spawn_local(async move {
        let result = match task.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(DeviceFailure::Failed(error)),
            Err(error) if error.is_panic() => Err(DeviceFailure::Panicked(error)),
            Err(_) => Err(DeviceFailure::Cancelled),
        };
        report.exit(result);
    });
    Ok(())
}

/// The runtime's view of a task: enough to stop it and to wait for it.
struct Supervised {
    name: Arc<str>,
    stop: Box<dyn Fn() + Send>,
    finished: Finished,
}

/// Places device tasks, supervises them, and owns the I/O thread.
pub struct DeviceRuntime {
    exits: mpsc::UnboundedSender<DeviceExit>,
    tasks: Vec<Supervised>,
    io_thread: Option<IoThread>,
}

impl DeviceRuntime {
    /// A runtime and the stream its tasks' unexpected exits arrive on.
    pub fn new() -> (Self, DeviceExits) {
        let (exits, receiver) = mpsc::unbounded_channel();
        let runtime = Self {
            exits,
            tasks: Vec::new(),
            io_thread: None,
        };
        (runtime, DeviceExits(receiver))
    }

    /// Spawns a device on this thread's local runtime. `device` is called
    /// inside the task, so the device may be `!Send` and a panic while
    /// constructing it is the task's.
    pub fn spawn<Q, D, F, E>(
        &mut self,
        name: impl Into<Arc<str>>,
        start: Start,
        device: D,
    ) -> Result<DeviceHandle<Q>, SpawnError>
    where
        Q: Send + 'static,
        D: FnOnce(Control<Q>) -> F + 'static,
        F: Future<Output = Result<(), E>> + 'static,
        E: Into<BoxError>,
    {
        let (handle, control, report) = self.prepare(name.into(), start);
        spawn_supervised(
            report,
            async move { device(control).await.map_err(Into::into) },
        )
        .map_err(|_| SpawnError::NoLocalRuntime)?;
        self.track(&handle);
        Ok(handle)
    }

    /// Spawns a device on the chosen thread. Only `factory` crosses threads;
    /// the device it builds there may be `!Send`.
    pub fn spawn_on<Q, D, F, E>(
        &mut self,
        name: impl Into<Arc<str>>,
        placement: Placement,
        start: Start,
        factory: D,
    ) -> Result<DeviceHandle<Q>, SpawnError>
    where
        Q: Send + 'static,
        D: FnOnce(Control<Q>) -> F + Send + 'static,
        F: Future<Output = Result<(), E>> + 'static,
        E: Into<BoxError>,
    {
        if placement == Placement::VmmMain {
            return self.spawn(name, start, factory);
        }
        let (handle, control, report) = self.prepare(name.into(), start);
        let io_thread = match self.io_thread.as_mut() {
            Some(io_thread) => io_thread,
            None => self.io_thread.insert(IoThread::start()?),
        };
        io_thread.run(Box::new(move || {
            let device = async move { factory(control).await.map_err(Into::into) };
            // The I/O thread always has a local runtime.
            if let Err(report) = spawn_supervised(report, device) {
                report.exit(Err(DeviceFailure::Cancelled));
            }
        }))?;
        self.track(&handle);
        Ok(handle)
    }

    /// Tasks that have not ended yet.
    pub fn running(&self) -> usize {
        self.tasks
            .iter()
            .filter(|task| !task.finished.is_done())
            .count()
    }

    /// Stops every task, waits up to `grace` for all of them to end, then
    /// joins the I/O thread. Tasks still running after `grace` are reported
    /// and left behind, and so is the I/O thread.
    pub async fn shutdown(mut self, grace: Duration) -> Result<(), ShutdownError> {
        self.stop_and_wait(Instant::now() + grace).await?;
        match self.io_thread.take() {
            Some(io_thread) => io_thread.join().await.map_err(ShutdownError::IoThread),
            None => Ok(()),
        }
    }

    /// Stops tasks and joins the I/O thread within one grace period, retaining
    /// completion receivers and the pending join across timeouts or cancellation.
    pub async fn shutdown_wait(&mut self, grace: Duration) -> Result<(), ShutdownError> {
        let deadline = Instant::now() + grace;
        self.stop_and_wait(deadline).await?;
        if let Some(io_thread) = &mut self.io_thread {
            match io_thread.join_wait(deadline).await {
                Err(IoThreadError::JoinTimeout(error)) => {
                    return Err(ShutdownError::IoJoinTimeout(error));
                }
                result => {
                    self.io_thread = None;
                    self.tasks.clear();
                    return result.map_err(ShutdownError::IoThread);
                }
            }
        }
        self.tasks.clear();
        Ok(())
    }

    async fn stop_and_wait(&mut self, deadline: Instant) -> Result<(), ShutdownError> {
        for task in &self.tasks {
            (task.stop)();
        }
        let mut unresponsive = Vec::new();
        for task in &mut self.tasks {
            if time::timeout_at(deadline, task.finished.wait())
                .await
                .is_err()
            {
                unresponsive.push(Arc::clone(&task.name));
            }
        }
        if unresponsive.is_empty() {
            Ok(())
        } else {
            Err(ShutdownError::Unresponsive(unresponsive))
        }
    }

    fn prepare<Q>(&self, name: Arc<str>, start: Start) -> (DeviceHandle<Q>, Control<Q>, Report) {
        let (commands, receiver) = mpsc::unbounded_channel();
        let (finished_tx, finished_rx) = watch::channel(());
        let stop_requested = Arc::new(AtomicBool::new(false));
        let handle = DeviceHandle {
            name: Arc::clone(&name),
            commands,
            stop_requested: Arc::clone(&stop_requested),
            finished: Finished(finished_rx),
        };
        let report = Report {
            name,
            stop_requested,
            exits: self.exits.clone(),
            _finished: finished_tx,
        };
        (
            handle,
            Control::new(receiver, start == Start::Parked),
            report,
        )
    }

    fn track<Q: Send + 'static>(&mut self, handle: &DeviceHandle<Q>) {
        self.tasks.retain(|task| !task.finished.is_done());
        let commands = handle.commands.downgrade();
        let stop_requested = Arc::clone(&handle.stop_requested);
        self.tasks.push(Supervised {
            name: Arc::clone(&handle.name),
            stop: Box::new(move || {
                stop_requested.store(true, Ordering::SeqCst);
                if let Some(commands) = commands.upgrade() {
                    let _ = commands.send(Command::Stop);
                }
            }),
            finished: handle.finished.clone(),
        });
    }
}

impl fmt::Debug for DeviceRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceRuntime")
            .field(
                "tasks",
                &self.tasks.iter().map(|task| &task.name).collect::<Vec<_>>(),
            )
            .field("io_thread", &self.io_thread.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests;
