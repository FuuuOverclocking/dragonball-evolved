use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::{future, io};

use logger::error_unlimited;
use tokio::sync::{mpsc, oneshot};
use virtio_queue::desc::split::Descriptor;
use virtio_queue::{Queue, QueueOwnedT, QueueT};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryBackend};

use super::super::config::ConfigPublisher;
use super::super::{Activation, ActivationRx, InterruptKind, IrqTrigger, Session};
use super::protocol::{self, Request, RequestType, Response};
use super::{
    HOST_QUEUE_SIZE, MemError, MemQuiesced, ResizeAdmission, ResizeError, ResizeRequest,
    RetryRequest, population_response, project,
};
use crate::devices::async_fd::AsyncEventFd;
use crate::devices::control::{Ack, Command, Control};
use crate::memory::hotplug::{BlockState, MemGeometry};
use crate::memory::service::{
    MemoryCompletion, MemoryOperationError, MemoryPhase, MemoryRequest, MemoryStatus,
    PendingMemory, SubmitError, WindowControl, WindowStatus,
};
use crate::memory::store::MemoryView;

/// One local-runtime owner; no shared device lock or cached host address.
pub struct MemDevice {
    geometry: MemGeometry,
    window: WindowControl,
    status: WindowStatus,
    publisher: ConfigPublisher,
    interrupt: Arc<IrqTrigger>,
    queue_kick: AsyncEventFd,
    activation: ActivationRx,
    resize: mpsc::Receiver<ResizeRequest>,
    retry: mpsc::Receiver<RetryRequest>,
    admission: Arc<Mutex<ResizeAdmission>>,
    state: MemQuiesced,
    restored_queue: Option<virtio_queue::QueueState>,
}

impl MemDevice {
    pub(super) fn new(
        geometry: MemGeometry,
        window: WindowControl,
        status: WindowStatus,
        publisher: ConfigPublisher,
        interrupt: Arc<IrqTrigger>,
        queue_kick: AsyncEventFd,
        activation: ActivationRx,
        resize: mpsc::Receiver<ResizeRequest>,
        retry: mpsc::Receiver<RetryRequest>,
        admission: Arc<Mutex<ResizeAdmission>>,
        state: MemQuiesced,
        restored_queue: Option<virtio_queue::QueueState>,
    ) -> Self {
        Self {
            geometry,
            window,
            status,
            publisher,
            interrupt,
            queue_kick,
            activation,
            resize,
            retry,
            admission,
            state,
            restored_queue,
        }
    }

    pub async fn run(mut self, control: Control<MemQuiesced>) -> Result<(), MemError> {
        let result = self.run_loop(control).await;
        self.admission
            .lock()
            .expect("virtio-mem admission lock poisoned")
            .stopped = true;
        if let Err(error) = &result {
            error_unlimited!(
                "virtio-mem worker failed (session needs reset): {}",
                crate::error::render_chain(error)
            );
        }
        result
    }

    async fn run_loop(&mut self, mut control: Control<MemQuiesced>) -> Result<(), MemError> {
        let mut run = Running::default();
        if self.restored_queue.is_some() {
            // Reject inconsistent restored rings before any control or guest-visible effect.
            let activation = self
                .activation
                .try_recv()
                .ok_or(MemError::RestoreActivation)?;
            run.active = Some(self.activate(activation)?);
            run.work = true;
        }
        loop {
            if run.stopping {
                self.admission
                    .lock()
                    .expect("virtio-mem admission lock poisoned")
                    .stopped = true;
            }
            // Biased control polling must not capture an already reset queue.
            if run
                .active
                .as_ref()
                .is_some_and(|active| active.session.reset_requested())
            {
                run.resetting = true;
                run.work = false;
            }
            if run.pending.is_none() {
                if matches!(run.controls.front(), Some(Command::Quiesce(_))) {
                    run.close(&mut self.resize, &self.admission);
                }
                while let Some(resize) = run.accepted.pop_front() {
                    if run.failure.is_some() {
                        let _ = resize.reply.send(Err(ResizeError::Stopped));
                    } else if let Err(error) = self.publish_resize(resize) {
                        run.fail(error);
                    }
                }
                if run.resetting {
                    release(&mut run.active);
                    run.resetting = false;
                    run.work = false;
                }
                if run.stopping {
                    release(&mut run.active);
                    return run.failure.map_or(Ok(()), Err);
                }
                if let Some(command) = run.controls.pop_front() {
                    match command {
                        Command::Quiesce(ack) => {
                            run.closing = false;
                            run.parked = true;
                            ack.send(self.capture(&run.active));
                        }
                        Command::Resume(ack) => run.resume(ack, &self.admission),
                        Command::Stop => unreachable!("Stop drains immediately"),
                    }
                    // A queued Resume cannot reopen admission in the Quiesce ACK transition.
                    tokio::task::yield_now().await;
                    continue;
                }
            }
            let can_work = !run.parked
                && !run.closing
                && !run.stopping
                && !run.resetting
                && run.pending.is_none()
                && run.active.is_some();
            tokio::select! {
                biased;
                command = control.recv(), if run.control_open => match command {
                    Command::Quiesce(ack) => {
                        if run.active.is_none() && let Some(activation) = self.activation.try_recv() {
                            match self.activate(activation) {
                                Ok(active) => run.active = Some(active),
                                Err(error) => run.fail(error),
                            }
                        }
                        if run.controls.is_empty() {
                            run.close(&mut self.resize, &self.admission);
                        }
                        run.controls.push_back(Command::Quiesce(ack));
                    }
                    Command::Resume(ack) => {
                        if run.closing || !run.controls.is_empty() {
                            run.controls.push_back(Command::Resume(ack));
                        } else {
                            run.resume(ack, &self.admission);
                        }
                    }
                    Command::Stop => {
                        self.admission.lock().expect("virtio-mem admission lock poisoned").stopped = true;
                        run.control_open = false;
                        run.close(&mut self.resize, &self.admission);
                        run.stopping = true;
                    }
                },
                () = reset_requested(&mut run.active), if !run.resetting => {
                    run.resetting = true;
                    run.work = false;
                },
                result = completion(&mut run.pending) => {
                    let pending = run.pending.take().expect("completion owns an accepted request");
                    if let Err(error) = self.complete(&mut run.active, pending, result) {
                        run.fail(error);
                    }
                    run.work = run.active.is_some();
                },
                retry = self.retry.recv(), if run.retry_open => match retry {
                    Some(retry) => {
                        let result = self.window.retry(retry.operation_id).map_err(ResizeError::Retry);
                        let _ = retry.reply.send(result);
                    }
                    None => run.retry_open = false,
                },
                status = self.status.changed(), if run.status_open => match status {
                    Ok(status) => {
                        if let Err(error) = self.observe(&status, run.pending.as_ref(), run.parked) {
                            run.fail(error);
                        }
                    }
                    Err(_) => {
                        run.status_open = false;
                        run.fail(MemError::StatusClosed);
                    }
                },
                activation = self.activation.recv(), if run.active.is_none() && run.activation_open && !run.stopping => match activation {
                    Some(activation) => match self.activate(activation) {
                        Ok(active) => {
                            run.started |= !run.parked;
                            run.active = Some(active);
                            run.work = true;
                        }
                        Err(error) => run.fail(error),
                    },
                    None => run.activation_open = false,
                },
                resize = self.resize.recv(), if run.resize_open && (run.accepted.is_empty() || run.parked || run.closing || run.stopping) => match resize {
                    Some(resize) => {
                        if run.stopping {
                            let _ = resize.reply.send(Err(ResizeError::Stopped));
                        } else if run.parked || run.closing {
                            let _ = resize.reply.send(Err(ResizeError::Paused));
                        } else {
                            run.started = true;
                            run.accepted.push_back(resize);
                        }
                    }
                    None => run.resize_open = false,
                },
                () = tokio::task::yield_now(), if can_work && run.work => {
                    run.started = true;
                    match self.process_one(run.active.as_mut().unwrap()) {
                        Ok(Step::Pending(pending)) => run.pending = Some(pending),
                        Ok(Step::Completed) => {},
                        Ok(Step::Idle(more)) => run.work = more,
                        Err(error) => run.fail(error),
                    }
                },
                kick = self.queue_kick.read(), if can_work && !run.work => {
                    match kick {
                        Ok(_) => run.work = true,
                        Err(error) => run.fail(MemError::Notification(error)),
                    }
                },
                else => future::pending::<()>().await,
            }
            // Return to control/reset/completion after each chain or empty-notification race.
            tokio::task::yield_now().await;
        }
    }

    fn activate(&mut self, activation: Activation) -> Result<Active, MemError> {
        let active = Active::new(activation, &self.interrupt, self.restored_queue)?;
        self.restored_queue = None;
        Ok(active)
    }

    fn capture(&self, active: &Option<Active>) -> MemQuiesced {
        MemQuiesced {
            queue: active.as_ref().map(|active| active.queue.state()),
            ..self.state
        }
    }

    fn observe(
        &mut self,
        status: &MemoryStatus,
        pending: Option<&InFlight>,
        parked: bool,
    ) -> Result<(), MemError> {
        if status.population.geometry() != self.geometry {
            return Err(MemError::GeometryMismatch);
        }
        match &status.phase {
            MemoryPhase::Stopping | MemoryPhase::Stopped => return Err(MemError::ServiceStopped),
            MemoryPhase::Changing(id) => {
                if pending.is_none_or(|pending| pending.memory.operation_id != *id) {
                    return Err(MemError::Ownership);
                }
            }
            MemoryPhase::Stalled(stall) => {
                if pending
                    .is_none_or(|pending| stall.operation_id != Some(pending.memory.operation_id))
                {
                    return Err(MemError::Ownership);
                }
            }
            _ => {}
        }
        if status.revision != self.state.memory_revision {
            let Some(pending) = pending.filter(|_| !parked) else {
                return Err(MemError::Ownership);
            };
            if self.state.memory_revision != pending.base_revision
                || pending.base_revision.checked_add(1) != Some(status.revision)
            {
                return Err(MemError::Ownership);
            }
            self.state.memory_revision = status.revision;
        }
        let config = project(self.geometry, self.state, status)?;
        if !parked {
            // Population commits advance generation, never CONFIG interrupts.
            // Phase-only changes leave both generation and interrupts untouched.
            self.publisher.publish(&config.encode())?;
        }
        Ok(())
    }

    fn publish_resize(&mut self, resize: ResizeRequest) -> Result<(), MemError> {
        let status = self.status.snapshot();
        self.observe(&status, None, false)?;
        let previous = project(self.geometry, self.state, &status)?;
        let config =
            match previous.with_requested_size(resize.requested_size, self.geometry.chunk_size()) {
                Ok(config) => config,
                Err(error) => {
                    let _ = resize.reply.send(Err(ResizeError::Config(error)));
                    return Ok(());
                }
            };
        if let Err(error) = self.publisher.publish(&config.encode()) {
            let _ = resize
                .reply
                .send(Err(ResizeError::Publication(error.clone())));
            return Err(MemError::ConfigImage(error));
        }
        self.state.requested_size = config.requested_size;
        self.state.usable_region_size = config.usable_region_size;
        if (previous.requested_size, previous.usable_region_size)
            != (config.requested_size, config.usable_region_size)
            && let Err(error) = self.interrupt.trigger(InterruptKind::Config)
        {
            let reported = io::Error::new(error.kind(), error.to_string());
            let _ = resize.reply.send(Err(ResizeError::Interrupt(reported)));
            return Err(MemError::Interrupt(error));
        }
        let _ = resize.reply.send(Ok(()));
        Ok(())
    }

    fn process_one(&mut self, active: &mut Active) -> Result<Step, MemError> {
        let status = self.status.snapshot();
        self.observe(&status, None, false)?;
        active.queue.disable_notification(active.mem.as_ref())?;
        let available = active
            .queue
            .avail_idx(active.mem.as_ref(), Ordering::Acquire)?
            .0;
        if available.wrapping_sub(active.queue.next_avail()) > active.queue.size() {
            return Err(MemError::AvailableIndex);
        }
        let Some(chain) = active.queue.iter(Arc::clone(&active.mem))?.next() else {
            if available != active.queue.next_avail() {
                return Err(MemError::Queue(virtio_queue::Error::InvalidChain));
            }
            return Ok(Step::Idle(
                active.queue.enable_notification(active.mem.as_ref())?,
            ));
        };
        let head = chain.head_index();
        // Keep only guest addresses and an owned wire request across the service await.
        // Read the two direct entries ourselves: DescriptorChain's iterator silently
        // truncates invalid chains and can follow unnegotiated indirect tables.
        let Some((bytes, response)) = response_space(active, head) else {
            finish(active, head, None)?;
            return Ok(Step::Completed);
        };
        let request = match Request::decode(&bytes) {
            Ok(request) => request,
            Err(_) => {
                finish(active, head, Some((response, Response::Error)))?;
                return Ok(Step::Completed);
            }
        };
        let config = project(self.geometry, self.state, &status)?;
        let range = match request.validate(&config) {
            Ok(range) => range,
            Err(_) => {
                finish(active, head, Some((response, Response::Error)))?;
                return Ok(Step::Completed);
            }
        };
        let memory_request = match request {
            Request::State { .. } => {
                let state = status
                    .population
                    .state(range.expect("STATE has a validated range"))
                    .map_err(|error| MemError::Operation(error.into()))?;
                let state = match state {
                    BlockState::Plugged => protocol::BlockState::Plugged,
                    BlockState::Unplugged => protocol::BlockState::Unplugged,
                    BlockState::Mixed => protocol::BlockState::Mixed,
                };
                finish(active, head, Some((response, Response::State(state))))?;
                return Ok(Step::Completed);
            }
            Request::Plug { .. } => MemoryRequest::Plug {
                range: range.expect("PLUG has a validated range"),
                target_limit: self.state.requested_size,
            },
            Request::Unplug { .. } => {
                MemoryRequest::Unplug(range.expect("UNPLUG has a validated range"))
            }
            Request::UnplugAll => MemoryRequest::UnplugAll,
        };
        match self.window.submit(memory_request) {
            Ok(memory) => Ok(Step::Pending(InFlight {
                memory,
                head,
                response,
                request_type: request.request_type(),
                base_revision: self.state.memory_revision,
            })),
            Err(SubmitError::Busy | SubmitError::Quiesced | SubmitError::QueueFull) => {
                finish(active, head, Some((response, Response::Busy)))?;
                Ok(Step::Completed)
            }
            Err(error) => Err(MemError::Submit(error)),
        }
    }

    fn complete(
        &mut self,
        active: &mut Option<Active>,
        pending: InFlight,
        result: Result<Result<MemoryCompletion, MemoryOperationError>, oneshot::error::RecvError>,
    ) -> Result<(), MemError> {
        let result = result.map_err(|_| MemError::ReplyDropped(pending.memory.operation_id))?;
        let status = self.status.snapshot();
        self.observe(&status, Some(&pending), false)?;
        let response = match result {
            Ok(completion) => {
                if completion.operation_id != pending.memory.operation_id
                    || completion.revision != status.revision
                    || (completion.revision != pending.base_revision
                        && pending.base_revision.checked_add(1) != Some(completion.revision))
                    || !Arc::ptr_eq(&completion.population, &status.population)
                {
                    return Err(MemError::Completion);
                }
                if pending.request_type == RequestType::UnplugAll {
                    if completion.population.plugged_size() != 0 {
                        return Err(MemError::Completion);
                    }
                    let config = project(self.geometry, self.state, &status)?
                        .after_unplug_all(self.geometry.chunk_size())?;
                    self.state.usable_region_size = config.usable_region_size;
                    self.publisher.publish(&config.encode())?;
                }
                Response::Ack
            }
            Err(MemoryOperationError::Population(error)) => {
                let response = population_response(error);
                if pending.request_type == RequestType::UnplugAll
                    || (response == Response::Nack && pending.request_type != RequestType::Plug)
                {
                    return Err(MemError::Operation(MemoryOperationError::Population(error)));
                }
                response
            }
            Err(error @ MemoryOperationError::RolledBack(_)) => {
                logger::warn!(
                    "virtio-mem operation {:?} rolled back: {}",
                    pending.memory.operation_id,
                    crate::error::render_chain(&error)
                );
                Response::Busy
            }
            Err(error) => return Err(MemError::Operation(error)),
        };
        // Config/population publication precedes ACK bytes, used index, then IRQ.
        let active = active.as_mut().ok_or(MemError::Completion)?;
        finish(active, pending.head, Some((pending.response, response)))
    }
}

struct Active {
    queue: Queue,
    mem: MemoryView,
    interrupt: Arc<IrqTrigger>,
    // Release the queue and RAM view before acknowledging reset on every exit path.
    session: Session,
}

impl Active {
    fn new(
        mut activation: Activation,
        interrupt: &Arc<IrqTrigger>,
        saved: Option<virtio_queue::QueueState>,
    ) -> Result<Self, MemError> {
        protocol::validate_features(activation.driver_features)?;
        if activation.queues.len() != 1 {
            return Err(MemError::QueueConfiguration);
        }
        let queue = &activation.queues[0];
        if queue.max_size() != protocol::QUEUE_MAX
            || queue.size() > protocol::QUEUE_MAX
            || !queue.is_valid(activation.mem.as_ref())
            || queue.event_idx_enabled()
            || !Arc::ptr_eq(interrupt, &activation.interrupt)
        {
            return Err(MemError::QueueConfiguration);
        }
        if saved.is_some_and(|saved| queue.state() != saved) {
            return Err(MemError::RestoreQueue);
        }
        let available = queue
            .avail_idx(activation.mem.as_ref(), Ordering::Acquire)?
            .0;
        let used = queue
            .used_idx(activation.mem.as_ref(), Ordering::Acquire)?
            .0;
        // Captures have no deferred head; a new driver starts with no used entries.
        if queue.next_avail() != queue.next_used()
            || used != queue.next_used()
            || (saved.is_none() && queue.next_used() != 0)
        {
            return Err(MemError::QueuePosition);
        }
        if available.wrapping_sub(queue.next_avail()) > queue.size() {
            return Err(MemError::AvailableIndex);
        }
        Ok(Self {
            queue: activation.queues.pop().unwrap(),
            mem: activation.mem,
            interrupt: activation.interrupt,
            session: activation.session,
        })
    }
}

struct InFlight {
    memory: PendingMemory,
    head: u16,
    response: GuestAddress,
    request_type: RequestType,
    base_revision: u64,
}

enum Step {
    Pending(InFlight),
    Completed,
    Idle(bool),
}

struct Running {
    active: Option<Active>,
    pending: Option<InFlight>,
    accepted: VecDeque<ResizeRequest>,
    controls: VecDeque<Command<MemQuiesced>>,
    parked: bool,
    started: bool,
    closing: bool,
    stopping: bool,
    resetting: bool,
    work: bool,
    failure: Option<MemError>,
    control_open: bool,
    resize_open: bool,
    retry_open: bool,
    status_open: bool,
    activation_open: bool,
}

impl Default for Running {
    fn default() -> Self {
        Self {
            active: None,
            pending: None,
            accepted: VecDeque::new(),
            controls: VecDeque::new(),
            parked: false,
            started: false,
            closing: false,
            stopping: false,
            resetting: false,
            work: false,
            failure: None,
            control_open: true,
            resize_open: true,
            retry_open: true,
            status_open: true,
            activation_open: true,
        }
    }
}

impl Running {
    fn resume(&mut self, ack: Ack<()>, admission: &Mutex<ResizeAdmission>) {
        self.started = true;
        self.closing = false;
        self.parked = false;
        self.work = self.active.is_some();
        admission
            .lock()
            .expect("virtio-mem admission lock poisoned")
            .paused = false;
        ack.send(());
    }

    fn close(
        &mut self,
        resize: &mut mpsc::Receiver<ResizeRequest>,
        admission: &Mutex<ResizeAdmission>,
    ) {
        let mut admission = admission
            .lock()
            .expect("virtio-mem admission lock poisoned");
        admission.paused = true;
        if !self.closing && !self.parked && self.started {
            // Preserve the running enqueue prefix, never publish before a parked start's Resume.
            for _ in 0..resize.len().min(HOST_QUEUE_SIZE) {
                if let Ok(resize) = resize.try_recv() {
                    self.accepted.push_back(resize);
                }
            }
        }
        self.closing = true;
        self.work = false;
    }

    fn fail(&mut self, error: MemError) {
        if self.failure.is_none() {
            self.failure = Some(error);
        }
        self.stopping = true;
        self.control_open = false;
        self.work = false;
        self.controls.clear();
        // Keep accepted memory ownership and retry alive until the operation settles.
    }
}

fn response_space(
    active: &Active,
    head: u16,
) -> Option<([u8; protocol::REQUEST_SIZE], GuestAddress)> {
    if head >= active.queue.size() {
        return None;
    }
    let table = GuestAddress(active.queue.desc_table());
    let header: Descriptor = active
        .mem
        .read_obj(table.checked_add(u64::from(head) * 16)?)
        .ok()?;
    if header.is_write_only()
        || header.refers_to_indirect_table()
        || !header.has_next()
        || header.len() < protocol::REQUEST_SIZE as u32
        || header.next() >= active.queue.size()
    {
        return None;
    }
    let response: Descriptor = active
        .mem
        .read_obj(table.checked_add(u64::from(header.next()) * 16)?)
        .ok()?;
    if !response.is_write_only()
        || response.refers_to_indirect_table()
        || response.has_next()
        || response.len() < protocol::RESPONSE_SIZE as u32
        || header.len().checked_add(response.len()).is_none()
        || !active.mem.check_range(header.addr(), header.len() as usize)
        || !active
            .mem
            .check_range(response.addr(), response.len() as usize)
    {
        return None;
    }
    let mut request = [0; protocol::REQUEST_SIZE];
    active.mem.read_slice(&mut request, header.addr()).ok()?;
    Some((request, response.addr()))
}

fn finish(
    active: &mut Active,
    head: u16,
    response: Option<(GuestAddress, Response)>,
) -> Result<(), MemError> {
    let len = if let Some((addr, response)) = response {
        active
            .mem
            .write_slice(&response.encode(), addr)
            .map_err(MemError::Response)?;
        protocol::RESPONSE_SIZE as u32
    } else {
        0
    };
    active.queue.add_used(active.mem.as_ref(), head, len)?;
    if active.queue.needs_notification(active.mem.as_ref())? {
        active
            .interrupt
            .trigger(InterruptKind::UsedRing)
            .map_err(MemError::Interrupt)?;
    }
    Ok(())
}

fn release(active: &mut Option<Active>) {
    if let Some(Active {
        queue,
        mem,
        interrupt,
        session,
    }) = active.take()
    {
        drop((queue, mem, interrupt));
        drop(session);
    }
}

async fn reset_requested(active: &mut Option<Active>) {
    match active {
        Some(active) => active.session.requested().await,
        None => future::pending().await,
    }
}

async fn completion(
    pending: &mut Option<InFlight>,
) -> Result<Result<MemoryCompletion, MemoryOperationError>, oneshot::error::RecvError> {
    match pending {
        Some(pending) => (&mut pending.memory.reply).await,
        None => future::pending().await,
    }
}
