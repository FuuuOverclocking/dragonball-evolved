//! The control protocol every device task speaks.
//!
//! A device task owns its [`Control`] and answers [`Command`]s inside its own
//! loop, so quiescing is cooperative: the device chooses the point at which it
//! parks, and reports what it was doing then.
//!
//! A typical loop polls the control first, and handles a command outside the
//! `select!`, so that nothing else runs while it is parked:
//!
//! ```ignore
//! loop {
//!     tokio::select! {
//!         biased;
//!         command = control.recv() => {
//!             if control.handle(command, || device.quiesced()).await == Flow::Stop {
//!                 return Ok(());
//!             }
//!         }
//!         event = device.next_event() => device.serve(event)?,
//!     }
//! }
//! ```

use tokio::sync::{mpsc, oneshot};

/// A request to a device task.
#[derive(Debug)]
pub enum Command<Q> {
    /// Reply with the device's quiesced state, then change no guest-visible
    /// state until `Resume` or `Stop`.
    Quiesce(Ack<Q>),
    /// Carry on from where the device parked; acknowledged once it does.
    Resume(Ack<()>),
    /// Finish up and return.
    Stop,
}

/// The reply slot of a [`Command`]. Sending never fails: a requester that
/// stopped waiting no longer cares.
#[derive(Debug)]
pub struct Ack<T>(Option<oneshot::Sender<T>>);

impl<T> Ack<T> {
    pub(crate) fn new(reply: oneshot::Sender<T>) -> Self {
        Self(Some(reply))
    }

    /// Nobody waits for it; stands in for the quiesce a parked task starts with.
    fn unobserved() -> Self {
        Self(None)
    }

    pub fn send(self, value: T) {
        if let Some(reply) = self.0 {
            let _ = reply.send(value);
        }
    }
}

/// Whether a device task carries on after a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Run,
    Stop,
}

/// A device task's end of its control channel.
///
/// Invariant, which every device upholds: once the task has answered
/// `Quiesce`, it changes no guest-visible state until it receives `Resume` or
/// `Stop`. [`Control::handle`] and [`Control::park`] uphold it for devices
/// that simply stop working while quiesced; a device whose work is invisible to
/// the guest, such as draining console output to the host, may answer and keep
/// going.
#[derive(Debug)]
pub struct Control<Q> {
    commands: mpsc::UnboundedReceiver<Command<Q>>,
    /// Delivered before anything from the channel.
    first: Option<Command<Q>>,
}

impl<Q> Control<Q> {
    /// A task spawned `parked` sees a `Quiesce` before any other command, so
    /// it does nothing guest-visible until its first `Resume`.
    pub(crate) fn new(commands: mpsc::UnboundedReceiver<Command<Q>>, parked: bool) -> Self {
        Self {
            commands,
            first: parked.then(|| Command::Quiesce(Ack::unobserved())),
        }
    }

    /// The next command. `Stop` once every requester is gone, since nobody
    /// could resume the task any more. Cancel-safe.
    pub async fn recv(&mut self) -> Command<Q> {
        if let Some(command) = self.first.take() {
            return command;
        }
        self.commands.recv().await.unwrap_or(Command::Stop)
    }
}

impl<Q: Clone> Control<Q> {
    /// Answers `command`: a `Quiesce` is answered with `quiesced()` and parks
    /// the task until `Resume` or `Stop`.
    pub async fn handle(&mut self, command: Command<Q>, quiesced: impl FnOnce() -> Q) -> Flow {
        match command {
            Command::Quiesce(ack) => {
                let state = quiesced();
                ack.send(state.clone());
                self.park(state).await
            }
            Command::Resume(ack) => {
                ack.send(());
                Flow::Run
            }
            Command::Stop => Flow::Stop,
        }
    }

    /// Waits, after having answered a `Quiesce` with `quiesced`, for `Resume`
    /// or `Stop`, answering repeated `Quiesce`s with the same state.
    pub async fn park(&mut self, quiesced: Q) -> Flow {
        loop {
            match self.recv().await {
                Command::Quiesce(ack) => ack.send(quiesced.clone()),
                Command::Resume(ack) => {
                    ack.send(());
                    return Flow::Run;
                }
                Command::Stop => return Flow::Stop,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(parked: bool) -> (mpsc::UnboundedSender<Command<u32>>, Control<u32>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (tx, Control::new(rx, parked))
    }

    fn quiesce(tx: &mpsc::UnboundedSender<Command<u32>>) -> oneshot::Receiver<u32> {
        let (reply, answer) = oneshot::channel();
        tx.send(Command::Quiesce(Ack::new(reply))).unwrap();
        answer
    }

    fn resume(tx: &mpsc::UnboundedSender<Command<u32>>) -> oneshot::Receiver<()> {
        let (reply, answer) = oneshot::channel();
        tx.send(Command::Resume(Ack::new(reply))).unwrap();
        answer
    }

    #[tokio::test]
    async fn quiesce_answers_and_parks_until_resume() {
        let (tx, mut control) = channel(false);
        let first = quiesce(&tx);
        let again = quiesce(&tx);
        let resumed = resume(&tx);
        let command = control.recv().await;
        assert_eq!(control.handle(command, || 7).await, Flow::Run);
        assert_eq!(first.await, Ok(7));
        assert_eq!(again.await, Ok(7));
        assert_eq!(resumed.await, Ok(()));

        let resumed = resume(&tx);
        let command = control.recv().await;
        assert_eq!(control.handle(command, || unreachable!()).await, Flow::Run);
        assert_eq!(resumed.await, Ok(()));
    }

    #[tokio::test]
    async fn stop_ends_a_parked_task_and_dropped_requesters_stop_it() {
        let (tx, mut control) = channel(false);
        let answer = quiesce(&tx);
        tx.send(Command::Stop).unwrap();
        let command = control.recv().await;
        assert_eq!(control.handle(command, || 1).await, Flow::Stop);
        assert_eq!(answer.await, Ok(1));

        drop(tx);
        assert!(matches!(control.recv().await, Command::Stop));
        assert_eq!(control.park(1).await, Flow::Stop);
    }

    #[tokio::test]
    async fn a_parked_start_waits_for_the_first_resume() {
        let (tx, mut control) = channel(true);
        let answer = quiesce(&tx);
        let resumed = resume(&tx);
        let command = control.recv().await;
        assert!(matches!(command, Command::Quiesce(_)));
        assert_eq!(control.handle(command, || 3).await, Flow::Run);
        assert_eq!(answer.await, Ok(3));
        assert_eq!(resumed.await, Ok(()));
    }
}
