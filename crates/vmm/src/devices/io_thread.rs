//! A thread with its own local runtime, on which device tasks are constructed.

use std::sync::mpsc as std_mpsc;
use std::{io, thread};

use tokio::runtime::LocalRuntime;
use tokio::sync::mpsc;
use tokio::task;

pub(super) type Job = Box<dyn FnOnce() + Send + 'static>;

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum IoThreadError {
    /// spawn the device I/O thread
    Spawn(#[source] io::Error),
    /// create the device I/O runtime
    Runtime(#[source] io::Error),
    /// device I/O thread exited during startup
    Startup,
    /// device I/O thread no longer accepts devices
    Closed,
    /// device I/O thread panicked
    Panicked,
    /// wait for the device I/O thread on the blocking pool
    Join(#[source] task::JoinError),
}

#[derive(Debug)]
pub(super) struct IoThread {
    // Jobs never hold a clone of this sender, so dropping it ends the thread's
    // loop once the queued jobs have run.
    jobs: mpsc::UnboundedSender<Job>,
    thread: thread::JoinHandle<()>,
}

impl IoThread {
    /// Starts the thread and waits for its runtime, but for no device work.
    pub(super) fn start() -> Result<Self, IoThreadError> {
        let (jobs, mut queued) = mpsc::unbounded_channel::<Job>();
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let thread = thread::Builder::new()
            .name("vmm-io".into())
            .spawn(move || {
                let runtime = match LocalRuntime::new() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }
                runtime.block_on(async move {
                    while let Some(job) = queued.recv().await {
                        job();
                    }
                });
            })
            .map_err(IoThreadError::Spawn)?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { jobs, thread }),
            Ok(Err(error)) => Err(IoThreadError::Runtime(error)),
            Err(_) => Err(IoThreadError::Startup),
        }
    }

    /// Runs `job` on the thread, inside its runtime.
    pub(super) fn run(&self, job: Job) -> Result<(), IoThreadError> {
        self.jobs.send(job).map_err(|_| IoThreadError::Closed)
    }

    /// Ends the thread once its queued jobs have run, dropping any task still
    /// on its runtime, and joins it.
    pub(super) async fn join(self) -> Result<(), IoThreadError> {
        let Self { jobs, thread } = self;
        drop(jobs);
        // Neither runtime teardown nor the join may block the caller's runtime.
        task::spawn_blocking(move || thread.join())
            .await
            .map_err(IoThreadError::Join)?
            .map_err(|_| IoThreadError::Panicked)
    }
}
