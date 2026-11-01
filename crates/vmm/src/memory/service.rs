use std::collections::VecDeque;
use std::future;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use super::hotplug::{
    MemGeometry, MemoryEffect, MemorySlice, Population, PopulationError, PopulationProposal,
};
use super::policy::SliceError;
use crate::devices::control::{Command, Control};
use crate::devices::runtime::{
    BoxError, DeviceHandle, DeviceRuntime, Placement, Response, SpawnError, Start,
};
use crate::space::{Gpa, Range};
use crate::vm::{BindError, Binding, KvmVm, SlotPermit, UnbindError};

const CHANNEL_CAPACITY: usize = 4;
const HELD: &str = "a chunk is taken only across a synchronous binding effect";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MemoryOperationId(pub u64);

#[derive(Debug)]
pub enum MemoryRequest {
    Plug {
        range: Range<Gpa>,
        target_limit: u64,
    },
    Unplug(Range<Gpa>),
    UnplugAll,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryQuiesced {
    pub population: Arc<Population>,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryCompletion {
    pub operation_id: MemoryOperationId,
    pub population: Arc<Population>,
    pub revision: u64,
}

#[derive(Debug)]
pub struct PendingMemory {
    pub operation_id: MemoryOperationId,
    pub reply: oneshot::Receiver<Result<MemoryCompletion, MemoryOperationError>>,
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum MemoryEffectError {
    /// bind a memory chunk
    Bind(#[source] BindError),
    /// unbind a memory chunk
    Unbind(#[source] UnbindError),
    /// discard removed blocks in {slice:?}
    Discard {
        slice: MemorySlice,
        #[source]
        source: BoxError,
    },
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum MemoryOperationError {
    /// reject an invalid population change
    Population(#[from] PopulationError),
    /// memory revision is exhausted
    RevisionExhausted,
    /// memory effect failed and all registration changes were rolled back
    RolledBack(#[source] Arc<MemoryEffectError>),
    /// memory service stopped and abandoned this operation at shutdown
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum SubmitError {
    /// memory service is quiescing or parked
    Quiesced,
    /// memory service already owns an accepted change
    Busy,
    /// memory service has stopped accepting work
    Stopped,
    /// memory service command queue is full
    QueueFull,
    /// memory operation identifiers are exhausted
    OperationIdExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum RetryError {
    /// memory service has no stalled operation
    NotStalled,
    /// operation {requested:?} is not the stalled operation {actual:?}
    WrongOperation {
        requested: MemoryOperationId,
        actual: MemoryOperationId,
    },
    /// send the memory retry
    Submit(#[from] SubmitError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryStage {
    Forward,
    Rollback,
    Cleanup,
}

#[derive(Debug, Clone)]
pub struct MemoryStall {
    pub generation: u64,
    pub operation_id: Option<MemoryOperationId>,
    pub stage: MemoryStage,
    pub effect: MemoryEffect,
    pub error: Arc<MemoryEffectError>,
}

#[derive(Debug, Clone)]
pub enum MemoryPhase {
    Prepared,
    Running,
    Changing(MemoryOperationId),
    Draining,
    Quiesced,
    Stalled(MemoryStall),
    Stopping,
    Stopped,
}

#[derive(Debug, Clone)]
pub struct MemoryStatus {
    pub population: Arc<Population>,
    pub revision: u64,
    pub phase: MemoryPhase,
}

#[derive(Debug, Clone)]
pub struct WindowStatus(watch::Receiver<MemoryStatus>);

impl WindowStatus {
    pub fn snapshot(&self) -> MemoryStatus {
        self.0.borrow().clone()
    }

    /// Wait for a new immutable projection; cancel-safe.
    pub async fn changed(&mut self) -> Result<MemoryStatus, watch::error::RecvError> {
        self.0.changed().await?;
        Ok(self.0.borrow_and_update().clone())
    }
}

#[derive(Debug, Default)]
struct Admission {
    closed: bool,
    busy: bool,
    terminal: bool,
}

type AdmissionGate = Arc<Mutex<Admission>>;

#[derive(Debug)]
pub struct WindowControl {
    requests: mpsc::Sender<Message>,
    admission: AdmissionGate,
    status: WindowStatus,
    next_id: u64,
}

impl WindowControl {
    pub fn submit(&mut self, request: MemoryRequest) -> Result<PendingMemory, SubmitError> {
        let mut admission = self
            .admission
            .lock()
            .expect("memory admission lock poisoned");
        if admission.terminal {
            return Err(SubmitError::Stopped);
        }
        if admission.closed {
            return Err(SubmitError::Quiesced);
        }
        if admission.busy {
            return Err(SubmitError::Busy);
        }
        let next = self
            .next_id
            .checked_add(1)
            .ok_or(SubmitError::OperationIdExhausted)?;
        let operation_id = MemoryOperationId(self.next_id);
        let (reply, answer) = oneshot::channel();
        self.requests
            .try_send(Message::Submit(OwnedRequest {
                operation_id,
                request,
                reply,
            }))
            .map_err(send_error)?;
        admission.busy = true;
        self.next_id = next;
        Ok(PendingMemory {
            operation_id,
            reply: answer,
        })
    }

    /// Authorize a retry of the observed stall; later failures need a fresh call.
    pub fn retry(&self, operation_id: MemoryOperationId) -> Result<(), RetryError> {
        let status = self.status.snapshot();
        let MemoryPhase::Stalled(stall) = status.phase else {
            return Err(RetryError::NotStalled);
        };
        let Some(actual) = stall.operation_id else {
            return Err(RetryError::NotStalled);
        };
        if actual != operation_id {
            return Err(RetryError::WrongOperation {
                requested: operation_id,
                actual,
            });
        }
        self.requests
            .try_send(Message::Retry {
                operation_id,
                generation: stall.generation,
            })
            .map_err(send_error)?;
        Ok(())
    }

    /// Only after CPUs stop; repeat to retry a stalled registration cleanup.
    pub fn stop(&self) -> Result<(), SubmitError> {
        let mut admission = self
            .admission
            .lock()
            .expect("memory admission lock poisoned");
        self.requests.try_send(Message::Stop).map_err(send_error)?;
        admission.closed = true;
        admission.terminal = true;
        Ok(())
    }
}

fn send_error<T>(error: mpsc::error::TrySendError<T>) -> SubmitError {
    match error {
        mpsc::error::TrySendError::Full(_) => SubmitError::QueueFull,
        mpsc::error::TrySendError::Closed(_) => SubmitError::Stopped,
    }
}

#[derive(Debug)]
pub struct MemoryHandle {
    device: DeviceHandle<MemoryQuiesced>,
    cleanup: mpsc::Sender<()>,
}

impl MemoryHandle {
    pub async fn quiesce(&self, within: Duration) -> Response<MemoryQuiesced> {
        self.device.quiesce(within).await
    }

    pub async fn resume(&self, within: Duration) -> Response<()> {
        self.device.resume(within).await
    }

    /// Only after CPUs stop; each call also retries any retained cleanup.
    pub fn stop(&self) {
        self.device.stop();
        let _ = self.cleanup.try_send(());
    }

    /// Only after CPUs stop; timeout or cancellation retains the same cleanup owner.
    pub async fn stop_wait(&self, within: Duration) -> Response<()> {
        self.stop();
        match tokio::time::timeout(within, self.finished()).await {
            Ok(()) => Response::Replied(()),
            Err(_) => Response::TimedOut,
        }
    }

    pub async fn finished(&self) {
        self.device.finished().await;
    }

    pub fn has_exited(&self) -> bool {
        self.device.has_exited()
    }
}

impl Drop for MemoryHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Debug)]
enum Message {
    Submit(OwnedRequest),
    Retry {
        operation_id: MemoryOperationId,
        generation: u64,
    },
    Stop,
}

#[derive(Debug)]
struct OwnedRequest {
    operation_id: MemoryOperationId,
    request: MemoryRequest,
    reply: oneshot::Sender<Result<MemoryCompletion, MemoryOperationError>>,
}

#[derive(Debug)]
enum Chunk {
    Free(SlotPermit),
    Bound(Binding),
}

#[derive(Debug)]
pub struct MemoryResources {
    vm: Arc<KvmVm>,
    chunks: Vec<Option<Chunk>>,
    #[cfg(test)]
    unbind_failures: usize,
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum ServiceSetupError {
    /// validate the memory service geometry
    Geometry(#[source] super::hotplug::GeometryError),
    /// population geometry does not describe this VM's window
    GeometryMismatch,
    /// memory service needs exactly {expected} chunk permits, not {actual}
    PermitCount { expected: u64, actual: usize },
    /// validate the permit for chunk {chunk}
    Permit {
        chunk: usize,
        #[source]
        source: BindError,
    },
    /// restore the populated binding for chunk {chunk}
    Restore {
        chunk: usize,
        #[source]
        source: BindError,
    },
}

/// Release memory service chunk {chunk}.
#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub struct MemoryReleaseError {
    pub chunk: usize,
    #[source]
    pub source: UnbindError,
}

impl MemoryResources {
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    pub fn bound_chunks(&self) -> usize {
        self.chunks
            .iter()
            .filter(|chunk| matches!(chunk, Some(Chunk::Bound(_))))
            .count()
    }

    pub fn release(mut self) -> Result<(), (Self, MemoryReleaseError)> {
        for chunk in 0..self.chunks.len() {
            if let Err(source) = self.unbind(chunk) {
                return Err((self, MemoryReleaseError { chunk, source }));
            }
        }
        Ok(())
    }

    fn bind(&mut self, chunk: usize, slice: MemorySlice) -> Result<(), BindError> {
        let held = self.chunks[chunk].take().expect(HELD);
        let Chunk::Free(permit) = held else {
            self.chunks[chunk] = Some(held);
            panic!("a proposed bind names a free chunk");
        };
        match self.vm.bind_slice(permit, slice.window(), slice.slice()) {
            Ok(binding) => {
                self.chunks[chunk] = Some(Chunk::Bound(binding));
                Ok(())
            }
            Err((permit, error)) => {
                self.chunks[chunk] = Some(Chunk::Free(permit));
                Err(error)
            }
        }
    }

    fn unbind(&mut self, chunk: usize) -> Result<(), UnbindError> {
        #[cfg(test)]
        if self.unbind_failures != 0 && matches!(self.chunks[chunk], Some(Chunk::Bound(_))) {
            self.unbind_failures -= 1;
            let Some(Chunk::Bound(binding)) = &self.chunks[chunk] else {
                unreachable!()
            };
            return Err(UnbindError {
                slot: binding.slot(),
                source: kvm_ioctls::Error::new(libc::EIO),
            });
        }
        let held = self.chunks[chunk].take().expect(HELD);
        match held {
            Chunk::Free(permit) => {
                self.chunks[chunk] = Some(Chunk::Free(permit));
                Ok(())
            }
            Chunk::Bound(binding) => match binding.unbind() {
                Ok((permit, mapping)) => {
                    drop(mapping);
                    self.chunks[chunk] = Some(Chunk::Free(permit));
                    Ok(())
                }
                Err((binding, error)) => {
                    self.chunks[chunk] = Some(Chunk::Bound(binding));
                    Err(error)
                }
            },
        }
    }

    fn validate_permits(&mut self, geometry: MemGeometry) -> Result<(), ServiceSetupError> {
        let empty = Range::new(geometry.slice().start(), 0).expect("empty range cannot overflow");
        for chunk in 0..self.chunks.len() {
            let Chunk::Free(permit) = self.chunks[chunk].take().expect(HELD) else {
                unreachable!()
            };
            // An empty slice checks VM ownership but cannot reach the registration ioctl.
            let (permit, error) = self
                .vm
                .bind_slice(permit, geometry.window_id(), empty)
                .unwrap_err();
            self.chunks[chunk] = Some(Chunk::Free(permit));
            if !matches!(error, BindError::Slice(SliceError::Empty(_))) {
                return Err(ServiceSetupError::Permit {
                    chunk,
                    source: error,
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct AcceptedChange {
    operation_id: MemoryOperationId,
    population: Population,
    effects: Vec<MemoryEffect>,
    next: usize,
    undo: Vec<MemoryEffect>,
    destructive: bool,
    rollback_error: Option<Arc<MemoryEffectError>>,
    reply: oneshot::Sender<Result<MemoryCompletion, MemoryOperationError>>,
}

#[derive(Debug)]
pub struct MemoryService {
    resources: MemoryResources,
    population: Arc<Population>,
    revision: u64,
    requests: mpsc::Receiver<Message>,
    cleanup: mpsc::Receiver<()>,
    cleanup_control: Option<mpsc::Sender<()>>,
    admission: AdmissionGate,
    status: watch::Sender<MemoryStatus>,
    queued: Option<OwnedRequest>,
    active: Option<AcceptedChange>,
    stall: Option<MemoryStall>,
    stall_generation: u64,
    pending_control: VecDeque<Command<MemoryQuiesced>>,
    activated: bool,
    pause_requested: bool,
    stopping: bool,
    cleanup_next: usize,
    #[cfg(test)]
    discard_failures: usize,
    #[cfg(test)]
    attempts: Vec<MemoryEffect>,
}

impl MemoryService {
    pub fn prepare(
        vm: Arc<KvmVm>,
        population: Population,
        permits: Vec<SlotPermit>,
    ) -> Result<(Self, WindowControl, WindowStatus), (MemoryResources, ServiceSetupError)> {
        Self::prepare_restored(vm, population, 0, permits)
    }

    pub fn prepare_restored(
        vm: Arc<KvmVm>,
        population: Population,
        revision: u64,
        permits: Vec<SlotPermit>,
    ) -> Result<(Self, WindowControl, WindowStatus), (MemoryResources, ServiceSetupError)> {
        let geometry = population.geometry();
        let mut resources = MemoryResources {
            vm,
            chunks: permits
                .into_iter()
                .map(|permit| Some(Chunk::Free(permit)))
                .collect(),
            #[cfg(test)]
            unbind_failures: 0,
        };
        let validate = || {
            let actual = MemGeometry::new(
                resources.vm.memory().layout(),
                geometry.window_id(),
                geometry.block_size(),
                geometry.chunk_size(),
                resources.vm.memory().policy().page_size,
            )
            .map_err(ServiceSetupError::Geometry)?;
            if actual != geometry {
                return Err(ServiceSetupError::GeometryMismatch);
            }
            if resources.chunks.len() as u64 != geometry.chunk_count() {
                return Err(ServiceSetupError::PermitCount {
                    expected: geometry.chunk_count(),
                    actual: resources.chunks.len(),
                });
            }
            Ok(())
        };
        if let Err(error) = validate() {
            return Err((resources, error));
        }
        if let Err(error) = resources.validate_permits(geometry) {
            return Err((resources, error));
        }
        for slice in population.populated_chunks() {
            let chunk = ordinal(geometry, slice);
            if let Err(source) = resources.bind(chunk, slice) {
                return Err((resources, ServiceSetupError::Restore { chunk, source }));
            }
        }
        let population = Arc::new(population);
        let (status, reader) = watch::channel(MemoryStatus {
            population: Arc::clone(&population),
            revision,
            phase: MemoryPhase::Prepared,
        });
        let reader = WindowStatus(reader);
        let admission = Arc::new(Mutex::new(Admission::default()));
        let (sender, requests) = mpsc::channel(CHANNEL_CAPACITY);
        let (cleanup_control, cleanup) = mpsc::channel(1);
        let window = WindowControl {
            requests: sender,
            admission: Arc::clone(&admission),
            status: reader.clone(),
            next_id: 1,
        };
        Ok((
            Self {
                resources,
                population,
                revision,
                requests,
                cleanup,
                cleanup_control: Some(cleanup_control),
                admission,
                status,
                queued: None,
                active: None,
                stall: None,
                stall_generation: 0,
                pending_control: VecDeque::new(),
                activated: false,
                pause_requested: false,
                stopping: false,
                cleanup_next: 0,
                #[cfg(test)]
                discard_failures: 0,
                #[cfg(test)]
                attempts: Vec::new(),
            },
            window,
            reader,
        ))
    }

    pub fn spawn(
        mut self,
        runtime: &mut DeviceRuntime,
        start: Start,
    ) -> Result<MemoryHandle, (Self, SpawnError)> {
        let cleanup = self
            .cleanup_control
            .take()
            .expect("a prepared service owns its cleanup sender");
        // Only the cold transfer is locked; the running future owns the service directly.
        let transfer = Arc::new(Mutex::new(Some(self)));
        let factory = Arc::clone(&transfer);
        match runtime.spawn_on("memory-service", Placement::Io, start, move |control| {
            let service = factory
                .lock()
                .expect("memory transfer lock poisoned")
                .take()
                .expect("memory service factory runs once");
            service.run(control, start)
        }) {
            Ok(device) => Ok(MemoryHandle { device, cleanup }),
            Err(error) => {
                let mut service = transfer
                    .lock()
                    .expect("memory transfer lock poisoned")
                    .take()
                    .expect("a rejected factory retains the memory service");
                service.cleanup_control = Some(cleanup);
                Err((service, error))
            }
        }
    }

    pub fn into_resources(mut self) -> MemoryResources {
        self.close(true);
        self.abandon();
        while let Ok(message) = self.requests.try_recv() {
            if let Message::Submit(request) = message {
                let _ = request.reply.send(Err(MemoryOperationError::Stopped));
            }
        }
        self.publish_phase(MemoryPhase::Stopped);
        self.resources
    }

    pub async fn run(
        mut self,
        mut control: Control<MemoryQuiesced>,
        start: Start,
    ) -> Result<(), BoxError> {
        let local = Rc::new(());
        drop(self.cleanup_control.take());
        self.activated = start == Start::Running;
        self.pause_requested = start == Start::Parked;
        if self.pause_requested {
            self.close(false);
        }
        self.publish();
        let mut control_open = true;
        let mut input_open = true;
        let mut cleanup_open = true;
        loop {
            self.flush_control();
            if self.stopping
                && self
                    .stall
                    .as_ref()
                    .is_some_and(|stall| stall.operation_id.is_some())
            {
                self.abandon();
                self.publish();
            }
            if self.stopping && !self.busy() && self.cleanup_next == self.resources.chunks.len() {
                self.publish_phase(MemoryPhase::Stopped);
                drop(local);
                return Ok(());
            }
            let advance = self.can_advance();
            let control_ready = self.control_ready();
            tokio::select! {
                biased;
                command = control.recv(), if control_open => {
                    if matches!(command, Command::Stop) {
                        // A closed Control synthesizes Stop forever; cleanup has its own retry capability.
                        control_open = false;
                    }
                    self.receive_control(command);
                },
                cleanup = self.cleanup.recv(), if cleanup_open => match cleanup {
                    Some(()) => self.begin_stop(),
                    None => cleanup_open = false,
                },
                message = self.requests.recv(), if input_open => match message {
                    Some(message) => self.receive(message),
                    None => input_open = false,
                },
                () = tokio::task::yield_now(), if advance || control_ready => {
                    if advance {
                        self.advance();
                    }
                },
                else => future::pending::<()>().await,
            }
        }
    }

    fn receive_control(&mut self, command: Command<MemoryQuiesced>) {
        if matches!(command, Command::Stop) {
            self.begin_stop();
            return;
        }
        if self.stopping {
            return;
        }
        match &command {
            Command::Quiesce(_) => {
                self.close(false);
                self.pause_requested = true;
            }
            Command::Resume(_) => {
                // The first Resume lets pre-transfer work drain without opening admission.
                self.activated = true;
            }
            Command::Stop => unreachable!(),
        }
        self.pending_control.push_back(command);
        self.publish();
    }

    fn receive(&mut self, message: Message) {
        match message {
            Message::Submit(request) => {
                if self.stopping && self.stall.is_some() {
                    let _ = request.reply.send(Err(MemoryOperationError::Stopped));
                    self.settled();
                } else {
                    assert!(
                        self.queued.is_none() && self.active.is_none(),
                        "one accepted memory change"
                    );
                    self.queued = Some(request);
                }
                self.publish();
            }
            Message::Retry {
                operation_id,
                generation,
            } => {
                if !self.stopping
                    && self.stall.as_ref().is_some_and(|stall| {
                        stall.operation_id == Some(operation_id) && stall.generation == generation
                    })
                {
                    self.stall = None;
                    self.publish();
                }
            }
            Message::Stop => self.begin_stop(),
        }
    }

    fn begin_stop(&mut self) {
        self.close(true);
        self.stopping = true;
        self.pending_control.clear();
        if self.stall.is_some() {
            self.abandon();
        }
        self.publish();
    }

    fn abandon(&mut self) {
        if let Some(change) = self.active.take() {
            let _ = change.reply.send(Err(MemoryOperationError::Stopped));
            self.settled();
        }
        if let Some(request) = self.queued.take() {
            let _ = request.reply.send(Err(MemoryOperationError::Stopped));
            self.settled();
        }
        self.stall = None;
    }

    fn busy(&self) -> bool {
        self.admission
            .lock()
            .expect("memory admission lock poisoned")
            .busy
    }

    fn close(&self, terminal: bool) {
        let mut admission = self
            .admission
            .lock()
            .expect("memory admission lock poisoned");
        admission.closed = true;
        admission.terminal |= terminal;
    }

    fn settled(&self) {
        self.admission
            .lock()
            .expect("memory admission lock poisoned")
            .busy = false;
    }

    fn control_ready(&self) -> bool {
        !self.stopping
            && !self.pending_control.is_empty()
            && !self.busy()
            && self.active.is_none()
            && self.queued.is_none()
            && self.stall.is_none()
    }

    fn flush_control(&mut self) {
        if !self.control_ready() {
            return;
        }
        match self.pending_control.pop_front().expect("a pending control") {
            Command::Quiesce(ack) => {
                self.pause_requested = true;
                self.close(false);
                self.publish();
                ack.send(MemoryQuiesced {
                    population: Arc::clone(&self.population),
                    revision: self.revision,
                });
            }
            Command::Resume(ack) => {
                self.activated = true;
                self.pause_requested = self
                    .pending_control
                    .iter()
                    .any(|command| matches!(command, Command::Quiesce(_)));
                self.admission
                    .lock()
                    .expect("memory admission lock poisoned")
                    .closed = self.pause_requested;
                self.publish();
                ack.send(());
            }
            Command::Stop => unreachable!("Stop supersedes pending controls"),
        }
    }

    fn can_advance(&self) -> bool {
        if self.stall.is_some() {
            return false;
        }
        if (self.activated || self.stopping) && (self.queued.is_some() || self.active.is_some()) {
            return true;
        }
        self.stopping && !self.busy() && self.cleanup_next < self.resources.chunks.len()
    }

    fn advance(&mut self) {
        if let Some(request) = self.queued.take() {
            let proposal = match request.request {
                MemoryRequest::Plug {
                    range,
                    target_limit,
                } => self.population.plug(range, target_limit),
                MemoryRequest::Unplug(range) => self.population.unplug(range),
                MemoryRequest::UnplugAll => Ok(self.population.unplug_all()),
            };
            let proposal = match proposal {
                Ok(proposal) => proposal,
                Err(error) => {
                    self.settled();
                    self.publish();
                    let _ = request.reply.send(Err(error.into()));
                    return;
                }
            };
            if proposal.effects().contains(&MemoryEffect::CommitPopulation)
                && self.revision == u64::MAX
            {
                self.settled();
                self.publish();
                let _ = request
                    .reply
                    .send(Err(MemoryOperationError::RevisionExhausted));
                return;
            }
            self.active = Some(AcceptedChange::new(
                request.operation_id,
                proposal,
                request.reply,
            ));
            self.publish();
            return;
        }
        if let Some(mut change) = self.active.take() {
            if let Some(original) = &change.rollback_error {
                if let Some(&effect) = change.undo.last() {
                    match self.effect(effect) {
                        Ok(()) => {
                            change.undo.pop();
                        }
                        Err(error) => {
                            self.record_stall(
                                Some(change.operation_id),
                                MemoryStage::Rollback,
                                effect,
                                error,
                            );
                        }
                    }
                    self.active = Some(change);
                } else {
                    self.settled();
                    self.publish();
                    let _ = change
                        .reply
                        .send(Err(MemoryOperationError::RolledBack(Arc::clone(original))));
                }
            } else if let Some(&effect) = change.effects.get(change.next) {
                if let MemoryEffect::Discard(_) = effect {
                    change.destructive = true;
                    change.undo.clear();
                }
                if effect == MemoryEffect::CommitPopulation {
                    assert_eq!(
                        change.next + 1,
                        change.effects.len(),
                        "commit is the final memory effect"
                    );
                    self.population = Arc::new(change.population);
                    self.revision += 1;
                    self.complete(change.operation_id, change.reply);
                } else {
                    match self.effect(effect) {
                        Ok(()) => {
                            change.next += 1;
                            match effect {
                                MemoryEffect::BindChunk(slice) => {
                                    change.undo.push(MemoryEffect::UnbindChunk(slice))
                                }
                                MemoryEffect::UnbindChunk(slice) => {
                                    change.undo.push(MemoryEffect::BindChunk(slice))
                                }
                                _ => {}
                            }
                        }
                        Err(error) if !change.destructive => {
                            change.rollback_error = Some(Arc::new(error))
                        }
                        Err(error) => {
                            self.record_stall(
                                Some(change.operation_id),
                                MemoryStage::Forward,
                                effect,
                                error,
                            );
                        }
                    }
                    self.active = Some(change);
                }
            } else {
                self.complete(change.operation_id, change.reply);
            }
            self.publish();
            return;
        }
        if self.stopping {
            while self.cleanup_next < self.resources.chunks.len() {
                let chunk = self.cleanup_next;
                if matches!(self.resources.chunks[chunk], Some(Chunk::Free(_))) {
                    self.cleanup_next += 1;
                    continue;
                }
                match self.resources.unbind(chunk) {
                    Ok(()) => self.cleanup_next += 1,
                    Err(error) => {
                        self.record_stall(
                            None,
                            MemoryStage::Cleanup,
                            MemoryEffect::UnbindChunk(
                                self.population
                                    .geometry()
                                    .chunk(chunk as u64)
                                    .expect("a reserved chunk"),
                            ),
                            MemoryEffectError::Unbind(error),
                        );
                    }
                }
                break;
            }
            self.publish();
        }
    }

    fn record_stall(
        &mut self,
        operation_id: Option<MemoryOperationId>,
        stage: MemoryStage,
        effect: MemoryEffect,
        error: MemoryEffectError,
    ) {
        self.stall_generation = self.stall_generation.wrapping_add(1);
        self.stall = Some(MemoryStall {
            generation: self.stall_generation,
            operation_id,
            stage,
            effect,
            error: Arc::new(error),
        });
    }

    fn complete(
        &mut self,
        operation_id: MemoryOperationId,
        reply: oneshot::Sender<Result<MemoryCompletion, MemoryOperationError>>,
    ) {
        self.settled();
        self.publish();
        let _ = reply.send(Ok(MemoryCompletion {
            operation_id,
            population: Arc::clone(&self.population),
            revision: self.revision,
        }));
    }

    fn effect(&mut self, effect: MemoryEffect) -> Result<(), MemoryEffectError> {
        #[cfg(test)]
        self.attempts.push(effect);
        match effect {
            MemoryEffect::BindChunk(slice) => self
                .resources
                .bind(ordinal(self.population.geometry(), slice), slice)
                .map_err(MemoryEffectError::Bind),
            MemoryEffect::UnbindChunk(slice) => self
                .resources
                .unbind(ordinal(self.population.geometry(), slice))
                .map_err(MemoryEffectError::Unbind),
            MemoryEffect::Discard(slice) => {
                self.resources
                    .vm
                    .memory()
                    .discard(slice.window(), slice.slice())
                    .map_err(|source| MemoryEffectError::Discard {
                        slice,
                        source: Box::new(source),
                    })?;
                #[cfg(test)]
                if self.discard_failures != 0 {
                    self.discard_failures -= 1;
                    return Err(MemoryEffectError::Discard {
                        slice,
                        source: Box::new(std::io::Error::from_raw_os_error(libc::EIO)),
                    });
                }
                Ok(())
            }
            MemoryEffect::CommitPopulation => {
                unreachable!("commit is a pure population transition")
            }
        }
    }

    fn publish(&self) {
        let phase = if let Some(stall) = &self.stall {
            MemoryPhase::Stalled(stall.clone())
        } else if self.stopping {
            MemoryPhase::Stopping
        } else if self.pause_requested && self.busy() {
            MemoryPhase::Draining
        } else if let Some(change) = &self.active {
            MemoryPhase::Changing(change.operation_id)
        } else if let Some(request) = &self.queued {
            MemoryPhase::Changing(request.operation_id)
        } else if self.pause_requested || !self.activated {
            MemoryPhase::Quiesced
        } else {
            MemoryPhase::Running
        };
        self.publish_phase(phase);
    }

    fn publish_phase(&self, phase: MemoryPhase) {
        self.status.send_replace(MemoryStatus {
            population: Arc::clone(&self.population),
            revision: self.revision,
            phase,
        });
    }
}

impl AcceptedChange {
    fn new(
        operation_id: MemoryOperationId,
        proposal: PopulationProposal,
        reply: oneshot::Sender<Result<MemoryCompletion, MemoryOperationError>>,
    ) -> Self {
        let (population, effects) = proposal.into_parts();
        Self {
            operation_id,
            population,
            effects,
            next: 0,
            undo: Vec::new(),
            destructive: false,
            rollback_error: None,
            reply,
        }
    }
}

fn ordinal(geometry: MemGeometry, slice: MemorySlice) -> usize {
    let ordinal =
        (slice.placement().start().0 - geometry.region().start().0) / geometry.chunk_size();
    usize::try_from(ordinal).expect("chunk permits fit the process address space")
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::path::Path;
    use std::task::{Context, Poll, Waker};

    use vm_memory::{Bytes, GuestAddress};

    use super::*;
    use crate::align::BackingPageSize;
    use crate::devices::control::Ack;
    use crate::kvm_context::KvmContext;
    use crate::memory::layout::{
        MemoryLayout, Object, ObjectId, Window, WindowId, WindowKind, WindowOwner,
    };
    use crate::memory::policy::{BackingKind, HostPolicy};
    use crate::memory::store::MemoryStore;
    use crate::space::ObjectOffset;
    use crate::vm::MemSlots;

    const HOT: WindowId = WindowId(1);
    const START: Gpa = Gpa(1 << 30);
    const BLOCK: u64 = 0x1000;
    const CHUNK: u64 = 4 * BLOCK;
    const SIZE: u64 = 3 * CHUNK;
    const TIMEOUT: Duration = Duration::from_secs(5);
    const KINDS: [BackingKind; 2] = [BackingKind::AnonymousPrivate, BackingKind::SharedMemfd];

    fn kvm() -> Option<KvmContext> {
        Path::new("/dev/kvm")
            .exists()
            .then(|| KvmContext::from_dev_path("/dev/kvm").unwrap())
    }

    fn range(first: u64, count: u64) -> Range<Gpa> {
        Range::new(Gpa(START.0 + first * BLOCK), count * BLOCK).unwrap()
    }

    fn vm(context: &KvmContext, kind: BackingKind) -> (Arc<KvmVm>, MemSlots, MemGeometry) {
        let boot = MemoryLayout::boot(4 * BLOCK).unwrap();
        let hot = Window::new(
            ObjectId(1),
            Range::new(ObjectOffset(BLOCK), SIZE).unwrap(),
            START,
            WindowOwner::VirtioMem,
            WindowKind::VirtioMemRam,
        )
        .unwrap();
        let layout = MemoryLayout::new(
            boot.objects().map(|(id, object)| (id, *object)).chain([(
                ObjectId(1),
                Object {
                    capacity: SIZE + BLOCK,
                },
            )]),
            boot.windows()
                .map(|(id, window)| (id, *window))
                .chain([(HOT, hot)]),
        )
        .unwrap();
        let policy = HostPolicy {
            kind,
            page_size: BackingPageSize::base(context.capabilities().page_size),
        };
        let geometry = MemGeometry::new(&layout, HOT, BLOCK, CHUNK, policy.page_size).unwrap();
        let memory = MemoryStore::materialize(layout, policy, context.capabilities()).unwrap();
        let (vm, slots) = KvmVm::new(context, memory).unwrap();
        (vm, slots, geometry)
    }

    fn prepared(
        context: &KvmContext,
        kind: BackingKind,
        ranges: &[Range<Gpa>],
    ) -> (Arc<KvmVm>, MemoryService, WindowControl, WindowStatus) {
        let (vm, mut slots, geometry) = vm(context, kind);
        let permits = slots.allocate(geometry.chunk_count() as usize).unwrap();
        drop(slots);
        let population = Population::restore(geometry, ranges.iter().copied()).unwrap();
        let (service, control, status) =
            MemoryService::prepare(Arc::clone(&vm), population, permits).unwrap();
        (vm, service, control, status)
    }

    fn enqueue(
        service: &mut MemoryService,
        control: &mut WindowControl,
        request: MemoryRequest,
    ) -> PendingMemory {
        let pending = control.submit(request).unwrap();
        let message = service.requests.try_recv().unwrap();
        service.receive(message);
        service.activated = true;
        pending
    }

    fn drain(service: &mut MemoryService) {
        for _ in 0..64 {
            if service.stall.is_some() || (service.active.is_none() && service.queued.is_none()) {
                return;
            }
            assert!(service.can_advance());
            service.advance();
        }
        panic!("memory change exceeded its finite effect list");
    }

    fn retry(
        service: &mut MemoryService,
        control: &WindowControl,
        operation_id: MemoryOperationId,
    ) {
        control.retry(operation_id).unwrap();
        let message = service.requests.try_recv().unwrap();
        service.receive(message);
    }

    fn request_quiesce(
        control: &mpsc::UnboundedSender<Command<MemoryQuiesced>>,
    ) -> oneshot::Receiver<MemoryQuiesced> {
        let (reply, answer) = oneshot::channel();
        control.send(Command::Quiesce(Ack::new(reply))).unwrap();
        answer
    }

    fn request_resume(
        control: &mpsc::UnboundedSender<Command<MemoryQuiesced>>,
    ) -> oneshot::Receiver<()> {
        let (reply, answer) = oneshot::channel();
        control.send(Command::Resume(Ack::new(reply))).unwrap();
        answer
    }

    fn marker(vm: &KvmVm, block: u64) -> u8 {
        vm.memory()
            .view()
            .read_obj(GuestAddress(range(block, 1).start().0))
            .unwrap()
    }

    fn mark(vm: &KvmVm, block: u64, byte: u8) {
        vm.memory()
            .view()
            .write_obj(byte, GuestAddress(range(block, 1).start().0))
            .unwrap();
    }

    #[tokio::test(flavor = "local")]
    async fn cancelled_waiter_does_not_cancel_an_owned_change_on_io_runtime() {
        let Some(context) = kvm() else { return };
        let (vm, service, mut control, status) =
            prepared(&context, BackingKind::AnonymousPrivate, &[]);
        let pending = control
            .submit(MemoryRequest::Plug {
                range: range(0, 1),
                target_limit: SIZE,
            })
            .unwrap();
        let first_id = pending.operation_id;
        drop(pending);
        let (mut runtime, _exits) = DeviceRuntime::new();
        let handle = service.spawn(&mut runtime, Start::Running).unwrap();
        let Response::Replied(capture) = handle.quiesce(TIMEOUT).await else {
            panic!("service did not drain")
        };
        assert_eq!(capture.population.plugged_size(), BLOCK);
        assert_eq!(capture.revision, 1);
        assert!(Arc::ptr_eq(
            &capture.population,
            &status.snapshot().population
        ));
        assert_eq!(handle.resume(TIMEOUT).await, Response::Replied(()));
        let pending = control.submit(MemoryRequest::UnplugAll).unwrap();
        assert!(pending.operation_id > first_id);
        pending.reply.await.unwrap().unwrap();
        handle.stop();
        handle.finished().await;
        assert!(matches!(status.snapshot().phase, MemoryPhase::Stopped));
        drop(vm.into_memory().unwrap());
        runtime.shutdown_wait(TIMEOUT).await.unwrap();
    }

    #[tokio::test(flavor = "local")]
    async fn parked_activation_defers_queued_effects_and_quiesce_drains_before_ack() {
        let Some(context) = kvm() else { return };
        let (vm, service, mut window, mut status) =
            prepared(&context, BackingKind::SharedMemfd, &[]);
        let mut pending = window
            .submit(MemoryRequest::Plug {
                range: range(3, 2),
                target_limit: SIZE,
            })
            .unwrap();
        let (mut runtime, _exits) = DeviceRuntime::new();
        let handle = service.spawn(&mut runtime, Start::Parked).unwrap();
        while !matches!(status.snapshot().phase, MemoryPhase::Draining) {
            status.changed().await.unwrap();
        }
        assert_eq!(status.snapshot().population.plugged_size(), 0);
        assert_eq!(status.snapshot().revision, 0);
        assert!(matches!(
            pending.reply.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let mut quiesce = std::pin::pin!(handle.quiesce(TIMEOUT));
        assert!(matches!(
            quiesce
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        let (capture, resumed) = tokio::join!(quiesce, handle.resume(TIMEOUT));
        let Response::Replied(capture) = capture else {
            panic!("queued change was not drained")
        };
        assert_eq!(capture.population.plugged_size(), 2 * BLOCK);
        assert_eq!(capture.revision, 1);
        assert_eq!(resumed, Response::Replied(()));
        pending.reply.await.unwrap().unwrap();
        assert!(matches!(
            handle.quiesce(TIMEOUT).await,
            Response::Replied(_)
        ));
        assert_eq!(
            window.submit(MemoryRequest::UnplugAll).unwrap_err(),
            SubmitError::Quiesced
        );
        assert_eq!(handle.resume(TIMEOUT).await, Response::Replied(()));
        let pending = window.submit(MemoryRequest::UnplugAll).unwrap();
        let (completion, capture) = tokio::join!(pending.reply, handle.quiesce(TIMEOUT));
        assert_eq!(completion.unwrap().unwrap().revision, 2);
        let Response::Replied(capture) = capture else {
            panic!("unplug was not drained")
        };
        assert_eq!(capture.population.plugged_size(), 0);
        handle.stop();
        handle.finished().await;
        drop(vm.into_memory().unwrap());
        runtime.shutdown_wait(TIMEOUT).await.unwrap();
    }

    #[test]
    fn mixed_chunk_unplug_preserves_other_bindings_and_discards_precisely_for_both_backings() {
        let Some(context) = kvm() else { return };
        for kind in KINDS {
            let (vm, mut service, mut control, status) =
                prepared(&context, kind, &[range(0, 2), range(4, 1)]);
            for (block, byte) in [(0, 0xa1), (1, 0xb2), (2, 0xc3), (4, 0xd4)] {
                mark(&vm, block, byte);
            }
            let mut pending = enqueue(
                &mut service,
                &mut control,
                MemoryRequest::Unplug(range(0, 1)),
            );
            drain(&mut service);
            assert_eq!(
                pending
                    .reply
                    .try_recv()
                    .unwrap()
                    .unwrap()
                    .population
                    .plugged_size(),
                2 * BLOCK
            );
            assert_eq!(service.resources.bound_chunks(), 2);
            assert!(
                !service
                    .attempts
                    .iter()
                    .any(|effect| matches!(effect, MemoryEffect::UnbindChunk(_)))
            );
            assert_eq!(
                (marker(&vm, 0), marker(&vm, 1), marker(&vm, 4)),
                (0, 0xb2, 0xd4)
            );
            let mut pending = enqueue(&mut service, &mut control, MemoryRequest::UnplugAll);
            drain(&mut service);
            pending.reply.try_recv().unwrap().unwrap();
            assert_eq!(service.resources.bound_chunks(), 0);
            assert_eq!(
                (marker(&vm, 1), marker(&vm, 2), marker(&vm, 4)),
                (0, 0xc3, 0)
            );
            assert_eq!(status.snapshot().revision, 2);
            service.into_resources().release().unwrap();
            drop(vm.into_memory().unwrap());
        }
    }

    #[test]
    fn partial_discard_retains_original_reply_and_retries_only_remaining_effects() {
        let Some(context) = kvm() else { return };
        for kind in KINDS {
            let (vm, mut service, mut control, status) =
                prepared(&context, kind, &[range(0, 1), range(4, 1)]);
            mark(&vm, 0, 0xa1);
            mark(&vm, 4, 0xb2);
            let mut pending = enqueue(&mut service, &mut control, MemoryRequest::UnplugAll);
            service.advance();
            service.advance();
            service.advance();
            service.advance();
            let first_discard = *service.attempts.last().unwrap();
            service.discard_failures = 2;
            service.advance();
            let stalled_discard = *service.attempts.last().unwrap();
            assert_ne!(first_discard, stalled_discard);
            assert_eq!((marker(&vm, 0), marker(&vm, 4)), (0, 0));
            assert_eq!(service.resources.bound_chunks(), 0);
            assert_eq!(status.snapshot().population.plugged_size(), 2 * BLOCK);
            assert_eq!(status.snapshot().revision, 0);
            assert!(matches!(
                pending.reply.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            let attempts = service.attempts.len();
            assert!(matches!(
                control.retry(MemoryOperationId(pending.operation_id.0 + 1)),
                Err(RetryError::WrongOperation { .. })
            ));
            assert_eq!(service.attempts.len(), attempts);
            let first_generation = service.stall.as_ref().unwrap().generation;
            control.retry(pending.operation_id).unwrap();
            control.retry(pending.operation_id).unwrap();
            let message = service.requests.try_recv().unwrap();
            service.receive(message);
            let delayed_retry = service.requests.try_recv().unwrap();
            service.advance();
            let second_generation = service.stall.as_ref().unwrap().generation;
            assert!(second_generation > first_generation);
            let attempts = service.attempts.len();
            service.receive(delayed_retry);
            assert_eq!(
                service.stall.as_ref().unwrap().generation,
                second_generation
            );
            assert!(!service.can_advance());
            assert_eq!(service.attempts.len(), attempts);
            assert!(matches!(
                pending.reply.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            retry(&mut service, &control, pending.operation_id);
            drain(&mut service);
            let completion = pending.reply.try_recv().unwrap().unwrap();
            assert_eq!(completion.operation_id, pending.operation_id);
            assert_eq!(completion.population.plugged_size(), 0);
            assert_eq!(completion.revision, 1);
            assert_eq!(
                service
                    .attempts
                    .iter()
                    .filter(|&&effect| effect == first_discard)
                    .count(),
                1
            );
            assert_eq!(
                service
                    .attempts
                    .iter()
                    .filter(|&&effect| effect == stalled_discard)
                    .count(),
                3
            );
            assert_eq!(
                service
                    .attempts
                    .iter()
                    .filter(|effect| matches!(effect, MemoryEffect::UnbindChunk(_)))
                    .count(),
                2
            );
            service.into_resources().release().unwrap();
            drop(vm.into_memory().unwrap());
        }
    }

    #[test]
    fn delayed_retry_cannot_authorize_a_later_effect_in_the_same_operation() {
        let Some(context) = kvm() else { return };
        let (vm, mut service, mut control, status) = prepared(
            &context,
            BackingKind::AnonymousPrivate,
            &[range(0, 1), range(4, 1)],
        );
        service.discard_failures = 1;
        let mut pending = enqueue(&mut service, &mut control, MemoryRequest::UnplugAll);
        drain(&mut service);
        let first = service.stall.as_ref().unwrap().clone();
        control.retry(pending.operation_id).unwrap();
        control.retry(pending.operation_id).unwrap();
        let retry = service.requests.try_recv().unwrap();
        let delayed_retry = service.requests.try_recv().unwrap();
        service.receive(retry);
        service.advance();
        service.discard_failures = 1;
        service.advance();
        let second = service.stall.as_ref().unwrap().clone();
        assert_eq!(first.operation_id, second.operation_id);
        assert_ne!(first.effect, second.effect);
        assert!(second.generation > first.generation);
        let attempts = service.attempts.len();
        let next = service.active.as_ref().unwrap().next;
        service.receive(delayed_retry);
        assert!(!service.can_advance());
        assert_eq!(
            service.stall.as_ref().unwrap().generation,
            second.generation
        );
        assert_eq!(service.active.as_ref().unwrap().next, next);
        assert_eq!(service.attempts.len(), attempts);
        assert_eq!(service.resources.bound_chunks(), 0);
        assert_eq!(status.snapshot().revision, 0);
        assert!(matches!(
            pending.reply.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        control.retry(pending.operation_id).unwrap();
        let retry = service.requests.try_recv().unwrap();
        service.receive(retry);
        drain(&mut service);
        let completion = pending.reply.try_recv().unwrap().unwrap();
        assert_eq!(completion.operation_id, pending.operation_id);
        assert_eq!(completion.revision, 1);
        assert_eq!(completion.population.plugged_size(), 0);
        service.into_resources().release().unwrap();
        drop(vm.into_memory().unwrap());
    }

    #[test]
    fn bind_failure_rolls_back_or_retains_the_actual_binding_until_retry() {
        let Some(context) = kvm() else { return };
        let (vm, mut slots, geometry) = vm(&context, BackingKind::AnonymousPrivate);
        let permits = slots.allocate(3).unwrap();
        let overlap = slots.allocate(1).unwrap().pop().unwrap();
        drop(slots);
        let second = geometry.chunk(1).unwrap();
        let overlap = overlap.bind_slice(HOT, second.slice()).unwrap();
        let (mut service, mut control, status) =
            MemoryService::prepare(Arc::clone(&vm), Population::new(geometry), permits).unwrap();
        let mut pending = enqueue(
            &mut service,
            &mut control,
            MemoryRequest::Plug {
                range: range(0, 8),
                target_limit: SIZE,
            },
        );
        service.advance();
        service.advance();
        service.advance();
        service.resources.unbind_failures = 2;
        service.advance();
        assert_eq!(service.resources.bound_chunks(), 1);
        let MemoryPhase::Stalled(stall) = status.snapshot().phase else {
            panic!("rollback must stall")
        };
        assert_eq!(stall.stage, MemoryStage::Rollback);
        assert_eq!(stall.operation_id, Some(pending.operation_id));
        assert!(matches!(
            pending.reply.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        retry(&mut service, &control, pending.operation_id);
        service.advance();
        assert_eq!(service.resources.bound_chunks(), 1);
        retry(&mut service, &control, pending.operation_id);
        drain(&mut service);
        let Err(MemoryOperationError::RolledBack(error)) = pending.reply.try_recv().unwrap() else {
            panic!("full rollback must reject")
        };
        assert!(
            matches!(error.as_ref(), MemoryEffectError::Bind(BindError::Kvm { source, .. }) if source.errno() == libc::EEXIST)
        );
        assert_eq!(service.resources.bound_chunks(), 0);
        assert_eq!(status.snapshot().population.plugged_size(), 0);
        assert_eq!(status.snapshot().revision, 0);
        service.into_resources().release().unwrap();
        let (permit, mapping) = overlap.unbind().unwrap();
        drop((permit, mapping));
        drop(vm.into_memory().unwrap());
    }

    #[test]
    fn unbind_failure_restores_original_permits_without_discarding_contents() {
        let Some(context) = kvm() else { return };
        let (vm, mut slots, geometry) = vm(&context, BackingKind::SharedMemfd);
        let permits = slots.allocate(3).unwrap();
        let alias = slots.allocate(1).unwrap().pop().unwrap();
        drop(slots);
        let population = Population::restore(geometry, [range(0, 1), range(4, 1)]).unwrap();
        let (mut service, mut control, status) =
            MemoryService::prepare(Arc::clone(&vm), population.clone(), permits).unwrap();
        let Some(Chunk::Bound(first)) = &service.resources.chunks[0] else {
            unreachable!()
        };
        let original_slot = first.slot();
        mark(&vm, 0, 0xa1);
        mark(&vm, 4, 0xb2);
        let mut pending = enqueue(&mut service, &mut control, MemoryRequest::UnplugAll);
        service.advance();
        service.advance();
        service.resources.unbind_failures = 1;
        service.advance();
        let alias = alias
            .bind_slice(HOT, geometry.chunk(0).unwrap().slice())
            .unwrap();
        service.advance();
        assert_eq!(service.resources.bound_chunks(), 1);
        assert!(matches!(status.snapshot().phase, MemoryPhase::Stalled(_)));
        retry(&mut service, &control, pending.operation_id);
        service.advance();
        assert!(matches!(
            pending.reply.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let (alias, mapping) = alias.unbind().unwrap();
        drop((alias, mapping));
        retry(&mut service, &control, pending.operation_id);
        drain(&mut service);
        assert!(matches!(
            pending.reply.try_recv().unwrap(),
            Err(MemoryOperationError::RolledBack(_))
        ));
        let Some(Chunk::Bound(first)) = &service.resources.chunks[0] else {
            unreachable!()
        };
        assert_eq!(first.slot(), original_slot);
        assert_eq!(service.resources.bound_chunks(), 2);
        assert_eq!(*status.snapshot().population, population);
        assert_eq!((marker(&vm, 0), marker(&vm, 4)), (0xa1, 0xb2));
        assert!(
            !service
                .attempts
                .iter()
                .any(|effect| matches!(effect, MemoryEffect::Discard(_)))
        );
        service.into_resources().release().unwrap();
        drop(vm.into_memory().unwrap());
    }

    #[test]
    fn setup_validates_all_permits_before_effects_and_retains_partial_restore_resources() {
        let Some(context) = kvm() else { return };
        for count in [0, 2, 4] {
            let (vm, mut slots, geometry) = vm(&context, BackingKind::AnonymousPrivate);
            let permits = slots.allocate(count).unwrap();
            drop(slots);
            let (resources, error) =
                MemoryService::prepare(Arc::clone(&vm), Population::new(geometry), permits)
                    .unwrap_err();
            assert!(
                matches!(error, ServiceSetupError::PermitCount { expected: 3, actual } if actual == count)
            );
            assert_eq!(resources.bound_chunks(), 0);
            resources.release().unwrap();
            drop(vm.into_memory().unwrap());
        }
        let (vm, mut slots, geometry) = vm(&context, BackingKind::AnonymousPrivate);
        let (foreign, mut foreign_slots, _) = self::vm(&context, BackingKind::AnonymousPrivate);
        let mut permits = slots.allocate(2).unwrap();
        permits.extend(foreign_slots.allocate(1).unwrap());
        drop((slots, foreign_slots));
        let population = Population::restore(geometry, [range(0, 1)]).unwrap();
        let (resources, error) =
            MemoryService::prepare(Arc::clone(&vm), population, permits).unwrap_err();
        assert!(matches!(
            error,
            ServiceSetupError::Permit {
                chunk: 2,
                source: BindError::ForeignPermit
            }
        ));
        assert_eq!(resources.bound_chunks(), 0);
        resources.release().unwrap();
        drop(vm.into_memory().unwrap());
        drop(foreign.into_memory().unwrap());

        let (vm, mut slots, geometry) = self::vm(&context, BackingKind::SharedMemfd);
        let permits = slots.allocate(3).unwrap();
        let overlap = slots
            .allocate(1)
            .unwrap()
            .pop()
            .unwrap()
            .bind_slice(HOT, geometry.chunk(1).unwrap().slice())
            .unwrap();
        drop(slots);
        let population = Population::restore(geometry, [range(0, 1), range(4, 1)]).unwrap();
        let (mut resources, error) =
            MemoryService::prepare_restored(Arc::clone(&vm), population, 7, permits).unwrap_err();
        assert!(matches!(error, ServiceSetupError::Restore { chunk: 1, .. }));
        assert_eq!(resources.chunk_count(), 3);
        assert_eq!(resources.bound_chunks(), 1);
        resources.unbind_failures = 2;
        for _ in 0..2 {
            let (retained, error) = resources.release().unwrap_err();
            assert_eq!(error.chunk, 0);
            assert_eq!(retained.bound_chunks(), 1);
            assert_eq!(retained.chunk_count(), 3);
            resources = retained;
        }
        resources.release().unwrap();
        let (permit, mapping) = overlap.unbind().unwrap();
        drop((permit, mapping));
        drop(vm.into_memory().unwrap());
    }

    #[tokio::test(flavor = "local")]
    async fn stop_retains_cleanup_ownership_after_device_consumer_is_gone() {
        let Some(context) = kvm() else { return };
        let (vm, mut service, window, mut status) =
            prepared(&context, BackingKind::AnonymousPrivate, &[range(0, 1)]);
        mark(&vm, 0, 0xa1);
        service.resources.unbind_failures = 2;
        let (mut runtime, _exits) = DeviceRuntime::new();
        let handle = service.spawn(&mut runtime, Start::Parked).unwrap();
        drop(window);
        handle.stop();
        while !matches!(status.snapshot().phase, MemoryPhase::Stalled(_)) {
            status.changed().await.unwrap();
        }
        assert!(!handle.has_exited());
        let MemoryPhase::Stalled(stall) = status.snapshot().phase else {
            unreachable!()
        };
        assert_eq!(stall.stage, MemoryStage::Cleanup);
        assert_eq!(stall.operation_id, None);
        assert_eq!(marker(&vm, 0), 0xa1);
        assert!(Arc::clone(&vm).into_memory().is_err());
        let first_generation = stall.generation;
        assert_eq!(
            handle.stop_wait(Duration::from_millis(1)).await,
            Response::TimedOut
        );
        loop {
            let state = status.changed().await.unwrap();
            if let MemoryPhase::Stalled(stall) = state.phase
                && stall.generation != first_generation
            {
                assert!(stall.generation > first_generation);
                assert_eq!(stall.stage, MemoryStage::Cleanup);
                break;
            }
        }
        assert!(!handle.has_exited());
        assert!(Arc::clone(&vm).into_memory().is_err());
        handle.stop();
        handle.finished().await;
        assert_eq!(marker(&vm, 0), 0xa1);
        drop(vm.into_memory().unwrap());
        runtime.shutdown_wait(TIMEOUT).await.unwrap();
    }

    #[tokio::test(flavor = "local")]
    async fn stop_abandons_a_destructive_stall_without_discarding_still_plugged_ram() {
        let Some(context) = kvm() else { return };
        let (vm, mut service, mut window, mut status) = prepared(
            &context,
            BackingKind::SharedMemfd,
            &[range(0, 1), range(4, 1)],
        );
        mark(&vm, 0, 0xa1);
        mark(&vm, 4, 0xb2);
        service.discard_failures = 1;
        let mut pending = window.submit(MemoryRequest::Unplug(range(0, 1))).unwrap();
        let (mut runtime, _exits) = DeviceRuntime::new();
        let handle = service.spawn(&mut runtime, Start::Running).unwrap();
        while !matches!(status.snapshot().phase, MemoryPhase::Stalled(_)) {
            status.changed().await.unwrap();
        }
        assert_eq!(marker(&vm, 0), 0);
        assert_eq!(marker(&vm, 4), 0xb2);
        assert!(matches!(
            pending.reply.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        handle.stop();
        handle.finished().await;
        assert!(matches!(
            pending.reply.await.unwrap(),
            Err(MemoryOperationError::Stopped)
        ));
        assert_eq!(marker(&vm, 4), 0xb2);
        assert!(matches!(status.snapshot().phase, MemoryPhase::Stopped));
        drop(vm.into_memory().unwrap());
        runtime.shutdown_wait(TIMEOUT).await.unwrap();
    }

    #[tokio::test(flavor = "local")]
    async fn stop_drains_an_accepted_change_before_releasing_its_vm_holders() {
        let Some(context) = kvm() else { return };
        let (vm, service, mut window, status) =
            prepared(&context, BackingKind::AnonymousPrivate, &[]);
        mark(&vm, 0, 0xa1);
        let pending = window
            .submit(MemoryRequest::Plug {
                range: range(0, 1),
                target_limit: SIZE,
            })
            .unwrap();
        let (mut runtime, _exits) = DeviceRuntime::new();
        let handle = service.spawn(&mut runtime, Start::Running).unwrap();
        assert_eq!(handle.stop_wait(TIMEOUT).await, Response::Replied(()));
        let completion = pending.reply.await.unwrap().unwrap();
        assert_eq!(completion.population.plugged_size(), BLOCK);
        assert_eq!(completion.revision, 1);
        assert!(matches!(status.snapshot().phase, MemoryPhase::Stopped));
        assert_eq!(marker(&vm, 0), 0xa1);
        drop(vm.into_memory().unwrap());
        runtime.shutdown_wait(TIMEOUT).await.unwrap();
    }

    #[tokio::test(flavor = "local")]
    async fn overlapping_control_barriers_acknowledge_quiesce_before_resume() {
        let Some(context) = kvm() else { return };
        let (vm, mut service, mut window, status) =
            prepared(&context, BackingKind::AnonymousPrivate, &[]);
        let mut pending = enqueue(
            &mut service,
            &mut window,
            MemoryRequest::Plug {
                range: range(0, 1),
                target_limit: SIZE,
            },
        );
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut control = Control::new(receiver, false);
        let mut first = request_quiesce(&sender);
        let mut repeated = request_quiesce(&sender);
        let mut resumed = request_resume(&sender);
        let mut last = request_quiesce(&sender);
        for _ in 0..4 {
            service.receive_control(control.recv().await);
            service.flush_control();
        }
        assert!(matches!(status.snapshot().phase, MemoryPhase::Draining));
        assert!(matches!(
            first.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            resumed.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(
            window.submit(MemoryRequest::UnplugAll).unwrap_err(),
            SubmitError::Quiesced
        );
        drain(&mut service);
        pending.reply.try_recv().unwrap().unwrap();

        service.flush_control();
        let capture = first.try_recv().unwrap();
        assert_eq!(capture.population.plugged_size(), BLOCK);
        assert_eq!(capture.revision, 1);
        assert!(matches!(status.snapshot().phase, MemoryPhase::Quiesced));
        assert!(service.pause_requested);
        assert!(matches!(
            repeated.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            resumed.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(
            window.submit(MemoryRequest::UnplugAll).unwrap_err(),
            SubmitError::Quiesced
        );

        service.flush_control();
        let again = repeated.try_recv().unwrap();
        assert_eq!(again, capture);
        assert!(Arc::ptr_eq(&again.population, &capture.population));
        assert!(matches!(
            resumed.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        service.flush_control();
        resumed.try_recv().unwrap();
        assert!(matches!(
            last.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(
            window.submit(MemoryRequest::UnplugAll).unwrap_err(),
            SubmitError::Quiesced
        );
        service.flush_control();
        assert_eq!(last.try_recv().unwrap(), capture);

        let mut repeated = request_quiesce(&sender);
        service.receive_control(control.recv().await);
        service.flush_control();
        assert_eq!(repeated.try_recv().unwrap(), capture);
        let mut resumed = request_resume(&sender);
        service.receive_control(control.recv().await);
        service.flush_control();
        resumed.try_recv().unwrap();
        assert!(matches!(status.snapshot().phase, MemoryPhase::Running));
        let pending = window.submit(MemoryRequest::UnplugAll).unwrap();
        service.into_resources().release().unwrap();
        assert!(matches!(
            pending.reply.await.unwrap(),
            Err(MemoryOperationError::Stopped)
        ));
        drop(vm.into_memory().unwrap());
    }

    #[tokio::test(flavor = "local")]
    async fn first_resume_drains_preaccepted_work_without_bypassing_quiesce() {
        let Some(context) = kvm() else { return };
        let (vm, mut service, mut window, status) =
            prepared(&context, BackingKind::AnonymousPrivate, &[]);
        let mut pending = window
            .submit(MemoryRequest::Plug {
                range: range(0, 1),
                target_limit: SIZE,
            })
            .unwrap();
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut control = Control::new(receiver, true);
        service.receive_control(control.recv().await);
        let request = service.requests.try_recv().unwrap();
        service.receive(request);
        assert!(!service.can_advance());
        assert!(service.attempts.is_empty());
        let mut quiesced = request_quiesce(&sender);
        let mut resumed = request_resume(&sender);
        service.receive_control(control.recv().await);
        service.receive_control(control.recv().await);
        assert!(service.can_advance());
        assert!(service.pause_requested);
        assert_eq!(
            window.submit(MemoryRequest::UnplugAll).unwrap_err(),
            SubmitError::Quiesced
        );
        drain(&mut service);
        assert_eq!(pending.reply.try_recv().unwrap().unwrap().revision, 1);
        service.flush_control();
        assert!(matches!(
            quiesced.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        service.flush_control();
        let capture = quiesced.try_recv().unwrap();
        assert_eq!(capture.population.plugged_size(), BLOCK);
        assert_eq!(capture.revision, 1);
        assert!(matches!(status.snapshot().phase, MemoryPhase::Quiesced));
        assert!(matches!(
            resumed.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(
            window.submit(MemoryRequest::UnplugAll).unwrap_err(),
            SubmitError::Quiesced
        );
        service.flush_control();
        resumed.try_recv().unwrap();
        assert!(matches!(status.snapshot().phase, MemoryPhase::Running));
        service.into_resources().release().unwrap();
        drop(vm.into_memory().unwrap());
    }

    #[tokio::test(flavor = "local")]
    async fn timed_out_controls_do_not_drop_later_acks_or_report_false_exit() {
        let Some(context) = kvm() else { return };
        let (vm, mut service, mut window, status) =
            prepared(&context, BackingKind::AnonymousPrivate, &[range(0, 1)]);
        service.discard_failures = 1;
        let pending = enqueue(&mut service, &mut window, MemoryRequest::UnplugAll);
        drain(&mut service);
        assert!(matches!(status.snapshot().phase, MemoryPhase::Stalled(_)));
        let cleanup = service.cleanup_control.take().unwrap();
        let (mut runtime, _exits) = DeviceRuntime::new();
        let device = runtime
            .spawn("memory-service", Start::Running, move |control| {
                service.run(control, Start::Running)
            })
            .unwrap();
        let handle = MemoryHandle { device, cleanup };
        let short = Duration::from_millis(1);
        for _ in 0..32 {
            assert_eq!(handle.quiesce(short).await, Response::TimedOut);
            assert_eq!(handle.resume(short).await, Response::TimedOut);
            assert!(!handle.has_exited());
        }
        window.retry(pending.operation_id).unwrap();
        let completion = pending.reply.await.unwrap().unwrap();
        assert_eq!(completion.operation_id, pending.operation_id);
        let Response::Replied(capture) = handle.quiesce(TIMEOUT).await else {
            panic!("later quiesce lost its acknowledgement")
        };
        assert_eq!(capture.revision, completion.revision);
        assert_eq!(capture.population.plugged_size(), 0);
        for _ in 0..32 {
            assert_eq!(
                handle.quiesce(TIMEOUT).await,
                Response::Replied(capture.clone())
            );
        }
        assert_eq!(handle.resume(TIMEOUT).await, Response::Replied(()));
        assert_eq!(handle.stop_wait(TIMEOUT).await, Response::Replied(()));
        drop(vm.into_memory().unwrap());
        runtime.shutdown_wait(TIMEOUT).await.unwrap();
    }
}
