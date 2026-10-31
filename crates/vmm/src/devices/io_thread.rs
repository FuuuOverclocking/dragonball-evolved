//! A thread with its own local runtime, on which device tasks are constructed.

use std::sync::mpsc as std_mpsc;
use std::{io, thread};

use tokio::runtime::LocalRuntime;
use tokio::sync::mpsc;
use tokio::task;
use tokio::time::{self, Instant};

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
    /// device I/O thread did not join in time
    JoinTimeout(#[source] time::error::Elapsed),
}

#[derive(Debug)]
pub(super) struct IoThread {
    state: JoinState,
}

#[derive(Debug)]
enum JoinState {
    Running {
        // Jobs never hold a clone of this sender, so dropping it ends the thread's
        // loop once the queued jobs have run.
        jobs: mpsc::UnboundedSender<Job>,
        thread: thread::JoinHandle<()>,
    },
    Joining(task::JoinHandle<thread::Result<()>>),
    Joined,
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
            Ok(Ok(())) => Ok(Self {
                state: JoinState::Running { jobs, thread },
            }),
            Ok(Err(error)) => Err(IoThreadError::Runtime(error)),
            Err(_) => Err(IoThreadError::Startup),
        }
    }

    /// Runs `job` on the thread, inside its runtime.
    pub(super) fn run(&self, job: Job) -> Result<(), IoThreadError> {
        match &self.state {
            JoinState::Running { jobs, .. } => jobs.send(job).map_err(|_| IoThreadError::Closed),
            JoinState::Joining(_) | JoinState::Joined => Err(IoThreadError::Closed),
        }
    }

    /// Ends the thread once its queued jobs have run, dropping any task still
    /// on its runtime, and joins it.
    pub(super) async fn join(mut self) -> Result<(), IoThreadError> {
        self.begin_join();
        match self.state {
            JoinState::Joining(join) => join
                .await
                .map_err(IoThreadError::Join)?
                .map_err(|_| IoThreadError::Panicked),
            JoinState::Joined => Ok(()),
            JoinState::Running { .. } => unreachable!(),
        }
    }

    /// A timeout retains the same join task for the next wait.
    pub(super) async fn join_wait(&mut self, deadline: Instant) -> Result<(), IoThreadError> {
        self.begin_join();
        let result = match &mut self.state {
            JoinState::Joining(join) => time::timeout_at(deadline, join)
                .await
                .map_err(IoThreadError::JoinTimeout)?,
            JoinState::Joined => return Ok(()),
            JoinState::Running { .. } => unreachable!(),
        };
        self.state = JoinState::Joined;
        result
            .map_err(IoThreadError::Join)?
            .map_err(|_| IoThreadError::Panicked)
    }

    fn begin_join(&mut self) {
        self.state = match std::mem::replace(&mut self.state, JoinState::Joined) {
            JoinState::Running { jobs, thread } => {
                drop(jobs);
                // Neither runtime teardown nor the join may block the caller's runtime.
                JoinState::Joining(task::spawn_blocking(move || thread.join()))
            }
            state => state,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "local")]
    async fn join_wait_reuses_its_join_task_and_records_completion_even_after_a_panic() {
        let within = std::time::Duration::from_secs(5);
        for panics in [false, true] {
            let mut io_thread = IoThread::start().unwrap();
            let (release_tx, release_rx) = std_mpsc::channel::<()>();
            io_thread
                .run(Box::new(move || {
                    release_rx.recv().unwrap();
                    if panics {
                        panic!("I/O thread join panic");
                    }
                }))
                .unwrap();

            let mut join_id = None;
            for _ in 0..2 {
                assert!(matches!(
                    io_thread.join_wait(Instant::now()).await,
                    Err(IoThreadError::JoinTimeout(_))
                ));
                let JoinState::Joining(join) = &io_thread.state else {
                    panic!("pending join was not retained");
                };
                assert_eq!(join.id(), *join_id.get_or_insert(join.id()));
            }

            release_tx.send(()).unwrap();
            let result = io_thread.join_wait(Instant::now() + within).await;
            if panics {
                assert!(matches!(result, Err(IoThreadError::Panicked)));
            } else {
                result.unwrap();
            }
            assert!(matches!(io_thread.state, JoinState::Joined));
            io_thread.join_wait(Instant::now()).await.unwrap();
        }
    }
}
