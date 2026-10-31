//! A mailbox that constructs device tasks on a dedicated local runtime.

use std::sync::mpsc as std_mpsc;
use std::thread;

use anyhow::{Context, Result, anyhow};
use tokio::runtime::LocalRuntime;
use tokio::sync::mpsc;
use tokio::task;

type Job = Box<dyn FnOnce() + Send + 'static>;

#[derive(Debug)]
pub(super) struct IoThread {
    // The receiver and device jobs never own a clone of this sender. Dropping
    // it after the last device exit is therefore sufficient to end the loop.
    tx: mpsc::UnboundedSender<Job>,
    thread: thread::JoinHandle<()>,
}

impl IoThread {
    pub(super) fn new() -> Result<Self> {
        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let thread = thread::Builder::new()
            .name("vmm-io".into())
            .spawn(move || {
                let runtime = match LocalRuntime::new().context("create device I/O local runtime") {
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
                    while let Some(job) = rx.recv().await {
                        job();
                    }
                });
            })
            .context("spawn device I/O thread")?;
        // Report initialization failures synchronously to `spawn_on`. This
        // handshake waits only for runtime creation, never for device work.
        ready_rx
            .recv()
            .context("wait for device I/O runtime initialization")??;
        Ok(Self { tx, thread })
    }

    pub(super) fn spawn(&self, job: Job) -> Result<()> {
        self.tx
            .send(job)
            .map_err(|_| anyhow!("device I/O mailbox is closed"))
    }

    pub(super) async fn shutdown(self) -> Result<()> {
        let Self { tx, thread } = self;
        drop(tx);
        // Runtime destruction can itself wait for blocking work. Neither that
        // nor the OS thread join may block the vmm's local executor.
        task::spawn_blocking(move || {
            thread
                .join()
                .map_err(|_| anyhow!("device I/O thread panicked"))
        })
        .await
        .context("join device I/O thread")?
    }
}
