//! What the main loop decides, apart from what it does.
//!
//! [`Controller`] is the machine's lifecycle as a value. For each input, a
//! function here matches it against the state exhaustively, answers what can
//! be answered at once, and returns a [`Next`] for the loop to carry out:
//! build a machine, hand it to a transition, or stop.
//!
//! A lifecycle operation (Start's first resume, Pause, Resume) owns the
//! machine as a local future until it finishes on its own; nothing here
//! cancels one. Generic over the running machine `R` and the paused machine
//! `P`, so the decisions are tested without KVM.

use std::error::Error;
use std::future::Future;
use std::pin::Pin;

use api::instance_info::{InstanceInfo, VmState};
use api::live_upgrade::LiveUpgradeState;
use api::logger::{LoggerConfig, UpdateLogger};
use api::{ApiResult, Reply, VmmCommand, VmmRequest};
use logger::{error_unlimited, info_unlimited, warn_unlimited};

use crate::VmmExitStatus;
use crate::devices::runtime::DeviceExit;
use crate::error::{api_error, render_chain};
use crate::machine::{
    BuildError, MachineEvent, MachineEvents, PauseError, PlanError, ResumeError, TeardownError,
    VcpuError,
};
use crate::process_ctx::UpdateLoggerFn;
use crate::spec::MachineSpec;
use crate::vmm_server::Inbound;

/// A lifecycle operation: a machine's own transition, polled by the loop
/// until it finishes. Its waits are bounded, so it always does.
pub(super) type Operation<T> = Pin<Box<dyn Future<Output = T>>>;

/// Why a request is refused in the state the machine is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub(super) enum Rejection {
    /// machine is not started
    NotStarted,
    /// machine is already started
    AlreadyStarted,
    /// machine configuration cannot change after Start
    ConfigurationFrozen,
    /// lifecycle operation in progress
    InProgress,
    /// live upgrade is not supported
    HandoverUnsupported,
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub(super) enum StartError {
    /// plan the machine
    Plan(#[source] PlanError),
    /// build the machine
    Build(#[source] BuildError),
    /// start the machine
    Resume(#[source] ResumeError),
    /// tear down the machine after a failed Start
    Teardown(#[source] TeardownError),
}

/// Why the vmm thread ends with an error.
#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub(super) enum ExitError {
    /// vCPU {index} failed: {error}
    VcpuFailed { index: u8, error: String },
    /// tear down the machine
    Teardown(#[source] TeardownError),
}

/// What answering requests needs from the process: everything but KVM.
#[derive(Clone, Copy)]
pub(super) struct Instance<'a> {
    pub(super) id: &'a str,
    pub(super) version: &'a str,
    pub(super) update_logger: UpdateLoggerFn,
}

impl Instance<'_> {
    fn info(&self, state: VmState) -> InstanceInfo {
        InstanceInfo {
            id: self.id.to_owned(),
            version: self.version.to_owned(),
            state,
        }
    }
}

/// A machine at rest in one state, and what it reports.
pub(super) struct Live<M> {
    pub(super) machine: M,
    pub(super) events: MachineEvents,
}

/// The machine's lifecycle, as the main loop holds it.
pub(super) enum Controller<R, P> {
    /// No machine yet; the configuration accumulates.
    NotStarted(MachineSpec),
    Running(Live<R>),
    Paused(Live<P>),
    /// An operation owns the machine until it finishes on its own.
    Busy(Busy<R, P>),
}

/// The lifecycle operation in flight.
pub(super) enum Busy<R, P> {
    Starting(Starting<R>),
    Pausing(Transition<R, P, PauseError>),
    Resuming(Transition<P, R, ResumeError>),
}

/// Start's first resume of a machine just built.
pub(super) struct Starting<R> {
    /// What the machine was built from, to start again if it fails.
    spec: MachineSpec,
    reply: Reply<ApiResult<()>>,
    /// Listened to by the loop, even during the first resume.
    pub(super) events: MachineEvents,
    /// Every client went away while the machine was starting.
    detached: bool,
    pub(super) operation: Operation<Result<R, StartError>>,
}

/// Pause or Resume: the machine moving from `F` to `T` inside its own future.
pub(super) struct Transition<F, T, E> {
    reply: Reply<ApiResult<()>>,
    /// Listened to while the machine moves.
    pub(super) events: MachineEvents,
    pub(super) operation: Operation<Result<T, (F, E)>>,
}

/// What a machine reported.
#[derive(Debug)]
pub(super) enum Event {
    Machine(MachineEvent),
    Device(DeviceExit),
}

/// Why the VMM stops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Exit {
    /// A client asked for it.
    Shutdown,
    /// Every client has gone, and no guest runs.
    Detached,
    /// The guest reset.
    GuestReset,
    /// A vCPU failed; `error` is its rendered source chain.
    VcpuFailed { index: u8, error: String },
    /// A failed Start could not tear down its machine; already rendered.
    StartTeardown(String),
    /// A vCPU transition failed without reporting an exit; already rendered.
    TransitionFailed(String),
}

/// What the loop does after an input.
pub(super) enum Next<R, P> {
    /// Go on serving.
    Stay(Controller<R, P>),
    /// Build a machine for the spec and start it; answer once it has started.
    Start(MachineSpec, Reply<ApiResult<()>>),
    /// Hand the running machine to a pause.
    Pause(Live<R>, Reply<ApiResult<()>>),
    /// Hand the paused machine to a resume.
    Resume(Live<P>, Reply<ApiResult<()>>),
    /// Stop taking requests, let the operation in flight finish, tear down
    /// what is left, and end the vmm thread.
    Exit(Controller<R, P>, Exit),
}

/// What is left to tear down once no operation holds the machine.
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Settled<R, P> {
    Nothing,
    Running(R),
    Paused(P),
}

/// A request, grouped by how the lifecycle treats it.
enum Request {
    GetInstanceInfo(Reply<InstanceInfo>),
    UpdateLogger(LoggerConfig, Reply<ApiResult<()>>),
    Configure(VmmCommand, Reply<ApiResult<()>>),
    Start(Reply<ApiResult<()>>),
    Pause(Reply<ApiResult<()>>),
    Resume(Reply<ApiResult<()>>),
    Shutdown(Reply<ApiResult<()>>),
    HandoverState(Reply<ApiResult<LiveUpgradeState>>),
}

impl From<VmmRequest> for Request {
    fn from(request: VmmRequest) -> Self {
        match request {
            VmmRequest::GetInstanceInfo(_, reply) => Self::GetInstanceInfo(reply),
            VmmRequest::UpdateLogger(UpdateLogger(config), reply) => {
                Self::UpdateLogger(config, reply)
            }
            VmmRequest::SetCpus(value, reply) => {
                Self::Configure(VmmCommand::SetCpus(value, ()), reply)
            }
            VmmRequest::SetMemory(value, reply) => {
                Self::Configure(VmmCommand::SetMemory(value, ()), reply)
            }
            VmmRequest::SetMemoryBacking(value, reply) => {
                Self::Configure(VmmCommand::SetMemoryBacking(value, ()), reply)
            }
            VmmRequest::SetConsole(value, reply) => {
                Self::Configure(VmmCommand::SetConsole(value, ()), reply)
            }
            VmmRequest::SetKernel(value, reply) => {
                Self::Configure(VmmCommand::SetKernel(value, ()), reply)
            }
            VmmRequest::AddDisk(value, reply) => {
                Self::Configure(VmmCommand::AddDisk(value, ()), reply)
            }
            VmmRequest::Start(_, reply) => Self::Start(reply),
            VmmRequest::Pause(_, reply) => Self::Pause(reply),
            VmmRequest::Resume(_, reply) => Self::Resume(reply),
            VmmRequest::Shutdown(_, reply) => Self::Shutdown(reply),
            VmmRequest::HandoverState(_, reply) => Self::HandoverState(reply),
        }
    }
}

impl<R, P> Controller<R, P> {
    /// What `GetInstanceInfo` reports: while an operation runs, the state it
    /// started from.
    fn state(&self) -> VmState {
        match self {
            Self::NotStarted(_) | Self::Busy(Busy::Starting(_)) => VmState::NotStarted,
            Self::Running(_) | Self::Busy(Busy::Pausing(_)) => VmState::Running,
            Self::Paused(_) | Self::Busy(Busy::Resuming(_)) => VmState::Paused,
        }
    }
}

impl<R> Starting<R> {
    pub(super) fn new(
        spec: MachineSpec,
        reply: Reply<ApiResult<()>>,
        events: MachineEvents,
        operation: Operation<Result<R, StartError>>,
    ) -> Self {
        Self {
            spec,
            reply,
            events,
            detached: false,
            operation,
        }
    }

    /// Answers the Start. Hands back the running machine with its events, or
    /// the spec of a machine that did not start and is gone.
    fn finish(self, result: Result<R, StartError>) -> Result<Live<R>, MachineSpec> {
        let Self {
            spec,
            reply,
            events,
            ..
        } = self;
        match result {
            Ok(machine) => {
                reply.send(Ok(()));
                Ok(Live { machine, events })
            }
            Err(error) => {
                warn_unlimited!("{}", render_chain(&error));
                reply.send(Err(api_error(&error)));
                Err(spec)
            }
        }
    }
}

impl<F, T, E: Error> Transition<F, T, E> {
    /// Hands the machine to `transition`, keeping its events for the loop.
    pub(super) fn new(
        from: Live<F>,
        reply: Reply<ApiResult<()>>,
        transition: impl FnOnce(F) -> Operation<Result<T, (F, E)>>,
    ) -> Self {
        Self {
            reply,
            events: from.events,
            operation: transition(from.machine),
        }
    }

    /// Answers the request. Hands the machine back with its events: moved, or
    /// where it was if the transition failed.
    ///
    /// The returned machine's type is authoritative. An exited vCPU reports
    /// why (reset or failure) through the events; a timeout is fatal rather
    /// than trusting a vCPU that no longer answers control requests.
    fn finish(self, result: Result<T, (F, E)>) -> Result<Live<T>, Live<F>> {
        let Self { reply, events, .. } = self;
        match result {
            Ok(machine) => {
                reply.send(Ok(()));
                Ok(Live { machine, events })
            }
            Err((machine, error)) => {
                warn_unlimited!("{}", render_chain(&error));
                reply.send(Err(api_error(&error)));
                Err(Live { machine, events })
            }
        }
    }
}

pub(super) fn on_inbound<R, P>(
    controller: Controller<R, P>,
    inbound: Inbound,
    instance: Instance<'_>,
) -> Next<R, P> {
    match inbound {
        Inbound::Request(request) => on_request(controller, request, instance),
        Inbound::Detached => on_detached(controller),
    }
}

pub(super) fn on_request<R, P>(
    controller: Controller<R, P>,
    request: VmmRequest,
    instance: Instance<'_>,
) -> Next<R, P> {
    match Request::from(request) {
        Request::GetInstanceInfo(reply) => {
            reply.send(instance.info(controller.state()));
            Next::Stay(controller)
        }
        Request::UpdateLogger(config, reply) => {
            reply.send((instance.update_logger)(config).map_err(|error| api_error(&*error)));
            Next::Stay(controller)
        }
        Request::HandoverState(reply) => {
            reply.send(Err(api_error(&Rejection::HandoverUnsupported)));
            Next::Stay(controller)
        }
        Request::Configure(command, reply) => match controller {
            Controller::NotStarted(spec) => match spec.apply(command) {
                Ok(next) => accept(Controller::NotStarted(next), reply),
                Err(error) => {
                    reply.send(Err(api_error(&error)));
                    Next::Stay(Controller::NotStarted(spec))
                }
            },
            started @ (Controller::Running(_) | Controller::Paused(_)) => {
                refuse(started, reply, Rejection::ConfigurationFrozen)
            }
            busy @ Controller::Busy(_) => refuse(busy, reply, Rejection::InProgress),
        },
        Request::Start(reply) => match controller {
            Controller::NotStarted(spec) => Next::Start(spec, reply),
            started @ (Controller::Running(_) | Controller::Paused(_)) => {
                refuse(started, reply, Rejection::AlreadyStarted)
            }
            busy @ Controller::Busy(_) => refuse(busy, reply, Rejection::InProgress),
        },
        Request::Pause(reply) => match controller {
            not_started @ Controller::NotStarted(_) => {
                refuse(not_started, reply, Rejection::NotStarted)
            }
            Controller::Running(running) => Next::Pause(running, reply),
            paused @ Controller::Paused(_) => accept(paused, reply),
            busy @ Controller::Busy(_) => refuse(busy, reply, Rejection::InProgress),
        },
        Request::Resume(reply) => match controller {
            not_started @ Controller::NotStarted(_) => {
                refuse(not_started, reply, Rejection::NotStarted)
            }
            running @ Controller::Running(_) => accept(running, reply),
            Controller::Paused(paused) => Next::Resume(paused, reply),
            busy @ Controller::Busy(_) => refuse(busy, reply, Rejection::InProgress),
        },
        Request::Shutdown(reply) => {
            // Accepted, not done: tearing down comes after, and how it went
            // shows only in the exit status.
            reply.send(Ok(()));
            Next::Exit(controller, Exit::Shutdown)
        }
    }
}

fn accept<R, P>(controller: Controller<R, P>, reply: Reply<ApiResult<()>>) -> Next<R, P> {
    reply.send(Ok(()));
    Next::Stay(controller)
}

fn refuse<R, P>(
    controller: Controller<R, P>,
    reply: Reply<ApiResult<()>>,
    rejection: Rejection,
) -> Next<R, P> {
    reply.send(Err(api_error(&rejection)));
    Next::Stay(controller)
}

pub(super) fn on_detached<R, P>(controller: Controller<R, P>) -> Next<R, P> {
    match controller {
        // Nobody is left to configure or start it.
        not_started @ Controller::NotStarted(_) => Next::Exit(not_started, Exit::Detached),
        Controller::Busy(Busy::Starting(starting)) => {
            warn_detached();
            Next::Stay(Controller::Busy(Busy::Starting(Starting {
                detached: true,
                ..starting
            })))
        }
        started @ (Controller::Running(_)
        | Controller::Paused(_)
        | Controller::Busy(Busy::Pausing(_) | Busy::Resuming(_))) => {
            warn_detached();
            Next::Stay(started)
        }
    }
}

fn warn_detached() {
    warn_unlimited!("every VMM client has gone; the guest keeps running as it is");
}

/// The same whatever the state: an operation in flight still finishes, on
/// the way out.
pub(super) fn on_event<R, P>(controller: Controller<R, P>, event: Event) -> Next<R, P> {
    match event {
        Event::Machine(MachineEvent::GuestReset) => {
            info_unlimited!("the guest reset; stopping the VMM");
            Next::Exit(controller, Exit::GuestReset)
        }
        Event::Machine(MachineEvent::VcpuFailed { index, error }) => {
            error_unlimited!("vCPU {index} failed; stopping the VMM");
            Next::Exit(controller, Exit::VcpuFailed { index, error })
        }
        Event::Device(exit) => {
            // Its supervisor has logged why. A local failure: the guest runs on.
            warn_unlimited!(
                "device {} has exited; the machine runs on without it",
                exit.name
            );
            Next::Stay(controller)
        }
    }
}

/// Hands a machine just built to its first resume, or answers why it could
/// not be built. Nothing of a failed build is left, so the same spec can be
/// fixed and started again.
pub(super) fn on_built<R, P>(
    spec: MachineSpec,
    reply: Reply<ApiResult<()>>,
    built: Result<(Operation<Result<R, StartError>>, MachineEvents), StartError>,
) -> Controller<R, P> {
    match built {
        Ok((operation, events)) => Controller::Busy(Busy::Starting(Starting::new(
            spec, reply, events, operation,
        ))),
        Err(error) => {
            warn_unlimited!("{}", render_chain(&error));
            reply.send(Err(api_error(&error)));
            Controller::NotStarted(spec)
        }
    }
}

pub(super) fn on_started<R, P>(starting: Starting<R>, result: Result<R, StartError>) -> Next<R, P> {
    let detached = starting.detached;
    let teardown = match &result {
        Err(error @ StartError::Teardown(_)) => Some(render_chain(error)),
        Ok(_) | Err(StartError::Plan(_) | StartError::Build(_) | StartError::Resume(_)) => None,
    };
    match starting.finish(result) {
        Err(spec) if teardown.is_some() => Next::Exit(
            Controller::NotStarted(spec),
            Exit::StartTeardown(teardown.unwrap()),
        ),
        Ok(running) => Next::Stay(Controller::Running(running)),
        // Nobody is left to start it again.
        Err(spec) if detached => Next::Exit(Controller::NotStarted(spec), Exit::Detached),
        Err(spec) => Next::Stay(Controller::NotStarted(spec)),
    }
}

pub(super) fn on_paused<R, P>(
    pausing: Transition<R, P, PauseError>,
    result: Result<P, (R, PauseError)>,
) -> Next<R, P> {
    let failure = result
        .as_ref()
        .err()
        .and_then(|(_, error)| transition_failure(error, &error.0));
    let state = match pausing.finish(result) {
        Ok(paused) => Controller::Paused(paused),
        Err(running) => Controller::Running(running),
    };
    match failure {
        Some(exit) => Next::Exit(state, exit),
        None => Next::Stay(state),
    }
}

pub(super) fn on_resumed<R, P>(
    resuming: Transition<P, R, ResumeError>,
    result: Result<R, (P, ResumeError)>,
) -> Next<R, P> {
    let failure = result
        .as_ref()
        .err()
        .and_then(|(_, error)| transition_failure(error, &error.0));
    let state = match resuming.finish(result) {
        Ok(running) => Controller::Running(running),
        Err(paused) => Controller::Paused(paused),
    };
    match failure {
        Some(exit) => Next::Exit(state, exit),
        None => Next::Stay(state),
    }
}

fn transition_failure(error: &dyn Error, vcpu: &VcpuError) -> Option<Exit> {
    match vcpu {
        // Its event distinguishes a normal guest reset from an internal fault.
        VcpuError::Exited(_) => None,
        VcpuError::TimedOut(_) | VcpuError::Panicked(_) | VcpuError::Join { .. } => {
            Some(Exit::TransitionFailed(render_chain(error)))
        }
    }
}

/// Lets the operation in flight, if any, finish on its own and answer its
/// request, then hands back the machine left to tear down. The VMM is
/// stopping, so a Start that fails is not offered again.
#[cfg(test)]
pub(super) async fn settle<R, P>(controller: Controller<R, P>) -> Settled<R, P> {
    match controller {
        Controller::NotStarted(_) => Settled::Nothing,
        Controller::Running(running) => Settled::Running(running.machine),
        Controller::Paused(paused) => Settled::Paused(paused.machine),
        Controller::Busy(Busy::Starting(mut starting)) => {
            let result = (&mut starting.operation).await;
            match starting.finish(result) {
                Ok(running) => Settled::Running(running.machine),
                Err(_) => Settled::Nothing,
            }
        }
        Controller::Busy(Busy::Pausing(mut pausing)) => {
            let result = (&mut pausing.operation).await;
            match pausing.finish(result) {
                Ok(paused) => Settled::Paused(paused.machine),
                Err(running) => Settled::Running(running.machine),
            }
        }
        Controller::Busy(Busy::Resuming(mut resuming)) => {
            let result = (&mut resuming.operation).await;
            match resuming.finish(result) {
                Ok(running) => Settled::Running(running.machine),
                Err(paused) => Settled::Paused(paused.machine),
            }
        }
    }
}

/// Teardown failure overrides the original exit reason: the accepted
/// Shutdown reply cannot report it, only the thread's final status can.
pub(super) fn exit_status(exit: Exit, teardown: Result<(), TeardownError>) -> VmmExitStatus {
    if let Err(error) = teardown {
        return VmmExitStatus::Error(render_chain(&ExitError::Teardown(error)));
    }
    match exit {
        Exit::Shutdown | Exit::Detached | Exit::GuestReset => VmmExitStatus::Ok,
        Exit::VcpuFailed { index, error } => {
            VmmExitStatus::Error(render_chain(&ExitError::VcpuFailed { index, error }))
        }
        Exit::StartTeardown(error) | Exit::TransitionFailed(error) => VmmExitStatus::Error(error),
    }
}

#[cfg(test)]
mod tests {
    use std::future::poll_fn;
    use std::io;
    use std::pin::pin;
    use std::task::Poll;

    use api::block::{AddDisk, BlockDevConfig};
    use api::boot::{KernelConfig, SetKernel};
    use api::instance_info::GetInstanceInfo;
    use api::lifecycle::{Pause, Resume, Shutdown, Start};
    use api::live_upgrade::HandoverState;
    use api::machine::{ConsoleConfig, MemorySizeMib, SetConsole, SetCpus, SetMemory, VcpuCount};
    use api::{ApiError, RawVmmOp};
    use tokio::sync::mpsc;

    use super::*;
    use crate::devices::runtime::DeviceRuntime;
    use crate::spec::SpecError;

    /// A running machine, as far as the decisions can tell.
    #[derive(Debug, PartialEq, Eq)]
    struct Running(u32);
    /// A paused machine, as far as the decisions can tell.
    #[derive(Debug, PartialEq, Eq)]
    struct Paused(u32);

    type Fake = Controller<Running, Paused>;
    type Answer = oneshot::Receiver<ApiResult<()>>;

    const INSTANCE: Instance<'static> = Instance {
        id: "test-id",
        version: "test-version",
        update_logger: |_| Ok(()),
    };

    /// apply the logger configuration
    #[derive(Debug, displaydoc::Display, thiserror::Error)]
    struct LoggerFailed(#[source] io::Error);

    fn make_request<Op: RawVmmOp>(op: Op) -> (VmmRequest, oneshot::Receiver<Op::Response>) {
        op.into_vmm_request()
    }

    fn make_reply() -> (Reply<ApiResult<()>>, Answer) {
        let (sender, receiver) = oneshot::channel();
        (sender.into(), receiver)
    }

    fn events() -> (mpsc::UnboundedSender<MachineEvent>, MachineEvents) {
        let (sender, machine) = mpsc::unbounded_channel();
        let (_, devices) = DeviceRuntime::new();
        (sender, MachineEvents { machine, devices })
    }

    fn live<M>(machine: M) -> Live<M> {
        Live {
            machine,
            events: events().1,
        }
    }

    /// An operation that finishes with what is sent to it.
    fn operation<T: 'static>() -> (oneshot::Sender<T>, Operation<T>) {
        let (sender, receiver) = oneshot::channel();
        let future = async move { receiver.await.expect("the operation was abandoned") };
        (sender, Box::pin(future))
    }

    fn spec_with_kernel() -> MachineSpec {
        MachineSpec::default()
            .apply(
                SetKernel(KernelConfig {
                    path: "/test/kernel".into(),
                    cmdline: None,
                    initrd: None,
                })
                .into_vmm_command(),
            )
            .unwrap()
    }

    fn mock_start() -> (oneshot::Sender<Result<Running, StartError>>, Answer, Fake) {
        let (finish, operation) = operation();
        let (reply, answer) = make_reply();
        let starting = Starting::new(spec_with_kernel(), reply, events().1, operation);
        (finish, answer, Controller::Busy(Busy::Starting(starting)))
    }

    #[allow(clippy::type_complexity)]
    fn mock_pause() -> (
        oneshot::Sender<Result<Paused, (Running, PauseError)>>,
        Answer,
        Fake,
    ) {
        let (finish, operation) = operation();
        let (reply, answer) = make_reply();
        let pausing = Transition::new(live(Running(1)), reply, |_| operation);
        (finish, answer, Controller::Busy(Busy::Pausing(pausing)))
    }

    #[allow(clippy::type_complexity)]
    fn mock_resume() -> (
        oneshot::Sender<Result<Running, (Paused, ResumeError)>>,
        Answer,
        Fake,
    ) {
        let (finish, operation) = operation();
        let (reply, answer) = make_reply();
        let resuming = Transition::new(live(Paused(1)), reply, |_| operation);
        (finish, answer, Controller::Busy(Busy::Resuming(resuming)))
    }

    fn stay(next: Next<Running, Paused>) -> Fake {
        match next {
            Next::Stay(controller) => controller,
            _ => panic!("expected to stay"),
        }
    }

    fn exit(next: Next<Running, Paused>) -> (Fake, Exit) {
        match next {
            Next::Exit(controller, exit) => (controller, exit),
            _ => panic!("expected to exit"),
        }
    }

    fn refused(rejection: Rejection) -> Result<ApiResult<()>, oneshot::TryRecvError> {
        Ok(Err(api_error(&rejection)))
    }

    fn info(controller: Fake) -> (Fake, VmState) {
        let (request, answer) = make_request(GetInstanceInfo);
        let controller = stay(on_request(controller, request, INSTANCE));
        let info = answer.try_recv().unwrap();
        assert_eq!((info.id.as_str(), info.version.as_str()), INSTANCE_ID);
        (controller, info.state)
    }

    const INSTANCE_ID: (&str, &str) = ("test-id", "test-version");

    /// Every request that changes the configuration or the lifecycle.
    fn writes() -> Vec<(&'static str, VmmRequest, Answer)> {
        let disk = AddDisk(BlockDevConfig {
            id: None,
            path: "/test/disk".into(),
            read_only: false,
            root: false,
        });
        let kernel = SetKernel(KernelConfig {
            path: "/test/kernel".into(),
            cmdline: None,
            initrd: None,
        });
        let mut writes = Vec::new();
        let mut add = |name, (request, answer)| writes.push((name, request, answer));
        add("SetCpus", make_request(SetCpus(VcpuCount(2))));
        add("SetMemory", make_request(SetMemory(MemorySizeMib(32))));
        add(
            "SetMemoryBacking",
            make_request(api::machine::SetMemoryBacking(
                api::machine::MemoryBacking::Shared,
            )),
        );
        add("SetConsole", make_request(SetConsole(ConsoleConfig::None)));
        add("SetKernel", make_request(kernel));
        add("AddDisk", make_request(disk));
        add("Start", make_request(Start));
        add("Pause", make_request(Pause));
        add("Resume", make_request(Resume));
        writes
    }

    #[test]
    fn configuration_applies_before_start_and_is_refused_after() {
        let (request, answer) = make_request(SetCpus(VcpuCount(2)));
        let controller = stay(on_request(
            Fake::NotStarted(MachineSpec::default()),
            request,
            INSTANCE,
        ));
        assert_eq!(answer.try_recv(), Ok(Ok(())));
        let (request, answer) = make_request(SetCpus(VcpuCount(0)));
        let controller = stay(on_request(controller, request, INSTANCE));
        assert_eq!(
            answer.try_recv(),
            Ok(Err(api_error(&SpecError::VcpuCount(0))))
        );
        let Controller::NotStarted(spec) = controller else {
            panic!("configuring started a machine");
        };
        assert_eq!(spec.cpus(), 2);

        for controller in [
            Fake::Running(live(Running(1))),
            Fake::Paused(live(Paused(1))),
        ] {
            let (request, answer) = make_request(SetMemory(MemorySizeMib(32)));
            let controller = stay(on_request(controller, request, INSTANCE));
            assert_eq!(answer.try_recv(), refused(Rejection::ConfigurationFrozen));
            let (request, answer) = make_request(Start);
            stay(on_request(controller, request, INSTANCE));
            assert_eq!(answer.try_recv(), refused(Rejection::AlreadyStarted));
        }
    }

    #[test]
    fn lifecycle_requests_move_the_machine_or_answer_at_once() {
        // Not started: Start takes the spec, Pause and Resume are refused.
        let spec = spec_with_kernel();
        for op in [make_request(Pause), make_request(Resume)] {
            let (request, answer) = op;
            let controller = stay(on_request(
                Fake::NotStarted(spec.clone()),
                request,
                INSTANCE,
            ));
            assert!(matches!(controller, Controller::NotStarted(_)));
            assert_eq!(answer.try_recv(), refused(Rejection::NotStarted));
        }
        let (request, answer) = make_request(Start);
        let next = on_request(Fake::NotStarted(spec.clone()), request, INSTANCE);
        match &next {
            Next::Start(started, _) => assert_eq!(started, &spec),
            _ => panic!("Start did not take the spec"),
        }
        // Answered once started.
        assert_eq!(answer.try_recv(), Err(oneshot::TryRecvError::Empty));

        // Running: Pause takes the machine, Resume is done already.
        let (request, answer) = make_request(Pause);
        let next = on_request(Fake::Running(live(Running(7))), request, INSTANCE);
        match &next {
            Next::Pause(running, _) => assert_eq!(running.machine, Running(7)),
            _ => panic!("Pause did not take the machine"),
        }
        assert_eq!(answer.try_recv(), Err(oneshot::TryRecvError::Empty));
        let (request, answer) = make_request(Resume);
        let controller = stay(on_request(
            Fake::Running(live(Running(7))),
            request,
            INSTANCE,
        ));
        assert!(matches!(controller, Controller::Running(_)));
        assert_eq!(answer.try_recv(), Ok(Ok(())));

        // Paused: the other way round.
        let (request, answer) = make_request(Resume);
        let next = on_request(Fake::Paused(live(Paused(7))), request, INSTANCE);
        match &next {
            Next::Resume(paused, _) => assert_eq!(paused.machine, Paused(7)),
            _ => panic!("Resume did not take the machine"),
        }
        assert_eq!(answer.try_recv(), Err(oneshot::TryRecvError::Empty));
        let (request, answer) = make_request(Pause);
        let controller = stay(on_request(Fake::Paused(live(Paused(7))), request, INSTANCE));
        assert!(matches!(controller, Controller::Paused(_)));
        assert_eq!(answer.try_recv(), Ok(Ok(())));
    }

    #[test]
    fn shutdown_is_accepted_in_every_state() {
        let (_start, _, starting) = mock_start();
        let (_pause, _, pausing) = mock_pause();
        let (_resume, _, resuming) = mock_resume();
        for controller in [
            Fake::NotStarted(MachineSpec::default()),
            Fake::Running(live(Running(1))),
            Fake::Paused(live(Paused(1))),
            starting,
            pausing,
            resuming,
        ] {
            let state = controller.state();
            let (request, answer) = make_request(Shutdown);
            let (controller, why) = exit(on_request(controller, request, INSTANCE));
            assert_eq!(answer.try_recv(), Ok(Ok(())));
            assert_eq!(why, Exit::Shutdown);
            // Whatever was in flight is still there, to finish on the way out.
            assert_eq!(controller.state(), state);
        }
    }

    #[test]
    fn busy_answers_queries_with_the_state_it_started_from() {
        let (_start, _, starting) = mock_start();
        let (_pause, _, pausing) = mock_pause();
        let (_resume, _, resuming) = mock_resume();
        for (controller, stable) in [
            (starting, VmState::NotStarted),
            (pausing, VmState::Running),
            (resuming, VmState::Paused),
        ] {
            let (controller, state) = info(controller);
            assert_eq!(state, stable);
            assert!(matches!(controller, Controller::Busy(_)));
        }
    }

    #[test]
    fn busy_refuses_configuration_and_lifecycle_requests() {
        let (_start, start_answer, starting) = mock_start();
        let (_pause, pause_answer, pausing) = mock_pause();
        let (_resume, resume_answer, resuming) = mock_resume();
        let mut kept = Vec::new();
        for mut controller in [starting, pausing, resuming] {
            for (name, request, answer) in writes() {
                controller = stay(on_request(controller, request, INSTANCE));
                assert_eq!(answer.try_recv(), refused(Rejection::InProgress), "{name}");
            }
            assert!(matches!(controller, Controller::Busy(_)));
            kept.push(controller);
        }
        assert_eq!(
            api_error(&Rejection::InProgress),
            ApiError::msg("lifecycle operation in progress")
        );
        // The operations in flight have not answered: nothing finished them.
        for answer in [start_answer, pause_answer, resume_answer] {
            assert_eq!(answer.try_recv(), Err(oneshot::TryRecvError::Empty));
        }
    }

    #[test]
    fn busy_keeps_updating_the_logger_and_refuses_handover() {
        let instance = Instance {
            update_logger: |_| Err(LoggerFailed(io::Error::other("target is gone")).into()),
            ..INSTANCE
        };
        let (_pause, _, pausing) = mock_pause();
        let (request, answer) = make_request(UpdateLogger(LoggerConfig::default()));
        let controller = stay(on_request(pausing, request, instance));
        // The error shows the update ran, and carries its whole chain.
        assert_eq!(
            answer.try_recv(),
            Ok(Err(ApiError::msg(
                "apply the logger configuration: target is gone"
            )))
        );
        let (request, answer) = make_request(UpdateLogger(LoggerConfig::default()));
        let controller = stay(on_request(controller, request, INSTANCE));
        assert_eq!(answer.try_recv(), Ok(Ok(())));
        let (request, answer) = make_request(HandoverState {});
        stay(on_request(controller, request, INSTANCE));
        assert_eq!(
            answer.try_recv().unwrap().unwrap_err(),
            api_error(&Rejection::HandoverUnsupported)
        );
    }

    #[tokio::test(flavor = "local")]
    async fn shutdown_while_busy_waits_for_the_operation_then_leaves_the_machine() {
        let (finish, pause_answer, pausing) = mock_pause();
        let (request, shutdown_answer) = make_request(Shutdown);
        let (controller, why) = exit(on_request(pausing, request, INSTANCE));
        assert_eq!(why, Exit::Shutdown);
        assert_eq!(shutdown_answer.try_recv(), Ok(Ok(())));

        // Not cancelled: settling waits for the pause to finish by itself.
        let mut settling = pin!(settle(controller));
        assert!(poll_fn(|cx| Poll::Ready(settling.as_mut().poll(cx).is_pending())).await);
        assert_eq!(pause_answer.try_recv(), Err(oneshot::TryRecvError::Empty));
        finish.send(Ok(Paused(1))).unwrap_or_else(|_| panic!());
        assert_eq!(settling.await, Settled::Paused(Paused(1)));
        // The pause answers for itself.
        assert_eq!(pause_answer.try_recv(), Ok(Ok(())));
    }

    #[tokio::test(flavor = "local")]
    async fn a_start_settling_on_the_way_out_answers_and_leaves_its_machine() {
        let (finish, answer, starting) = mock_start();
        let mut settling = pin!(settle(starting));
        assert!(poll_fn(|cx| Poll::Ready(settling.as_mut().poll(cx).is_pending())).await);
        finish.send(Ok(Running(3))).unwrap_or_else(|_| panic!());
        assert_eq!(settling.await, Settled::Running(Running(3)));
        assert_eq!(answer.try_recv(), Ok(Ok(())));

        // One that fails has torn its machine down itself.
        let (finish, answer, starting) = mock_start();
        let error = StartError::Resume(ResumeError(VcpuError::TimedOut(0)));
        finish.send(Err(error)).unwrap_or_else(|_| panic!());
        assert_eq!(settle(starting).await, Settled::Nothing);
        assert_eq!(
            answer.try_recv(),
            Ok(Err(ApiError::msg(
                "start the machine: resume the vCPUs: vCPU 0 did not answer in time"
            )))
        );
    }

    #[test]
    fn machine_events_stop_the_vmm_and_device_exits_do_not() {
        let (_pause, _, pausing) = mock_pause();
        for controller in [
            Fake::Running(live(Running(1))),
            Fake::Paused(live(Paused(1))),
            pausing,
        ] {
            let state = controller.state();
            let controller = stay(on_event(
                controller,
                Event::Device(DeviceExit {
                    name: "console-input".into(),
                    result: Ok(()),
                }),
            ));
            assert_eq!(controller.state(), state);
            let (controller, why) = exit(on_event(
                controller,
                Event::Machine(MachineEvent::GuestReset),
            ));
            assert_eq!(why, Exit::GuestReset);
            assert_eq!(controller.state(), state);
        }
        let failed = MachineEvent::VcpuFailed {
            index: 1,
            error: "KVM reported an internal error".into(),
        };
        let (_, why) = exit(on_event(
            Fake::Running(live(Running(1))),
            Event::Machine(failed),
        ));
        assert_eq!(
            exit_status(why, Ok(())),
            VmmExitStatus::Error("vCPU 1 failed: KVM reported an internal error".into())
        );
    }

    #[tokio::test(flavor = "local")]
    async fn a_reset_while_a_pause_is_in_flight_ends_with_ok_even_if_the_pause_fails() {
        let (finish, pause_answer, pausing) = mock_pause();
        let (controller, why) = exit(on_event(pausing, Event::Machine(MachineEvent::GuestReset)));
        // The vCPU that reset has exited, so the pause fails.
        let failed = (Running(1), PauseError(VcpuError::Exited(0)));
        finish.send(Err(failed)).unwrap_or_else(|_| panic!());
        assert_eq!(settle(controller).await, Settled::Running(Running(1)));
        assert_eq!(
            pause_answer.try_recv(),
            Ok(Err(ApiError::msg("pause the vCPUs: vCPU 0 has exited")))
        );
        assert_eq!(exit_status(why, Ok(())), VmmExitStatus::Ok);
    }

    #[test]
    fn a_failed_transition_answers_and_keeps_the_machine_where_it_came_back() {
        let (_, pause_answer, pausing) = mock_pause();
        let Controller::Busy(Busy::Pausing(pausing)) = pausing else {
            unreachable!()
        };
        let failed = (Running(2), PauseError(VcpuError::TimedOut(1)));
        let (controller, why) = exit(on_paused(pausing, Err(failed)));
        assert_eq!(
            exit_status(why, Ok(())),
            VmmExitStatus::Error("pause the vCPUs: vCPU 1 did not answer in time".into())
        );
        let Controller::Running(running) = controller else {
            panic!("a failed pause did not come back running");
        };
        assert_eq!(running.machine, Running(2));
        assert_eq!(
            pause_answer.try_recv(),
            Ok(Err(ApiError::msg(
                "pause the vCPUs: vCPU 1 did not answer in time"
            )))
        );

        let (_, resume_answer, resuming) = mock_resume();
        let Controller::Busy(Busy::Resuming(resuming)) = resuming else {
            unreachable!()
        };
        let failed = (Paused(2), ResumeError(VcpuError::Exited(0)));
        let controller = stay(on_resumed(resuming, Err(failed)));
        assert!(matches!(
            controller,
            Controller::Paused(Live {
                machine: Paused(2),
                ..
            })
        ));
        assert_eq!(
            resume_answer.try_recv(),
            Ok(Err(ApiError::msg("resume the vCPUs: vCPU 0 has exited")))
        );

        // And the successful ones move it.
        let (_, answer, pausing) = mock_pause();
        let Controller::Busy(Busy::Pausing(pausing)) = pausing else {
            unreachable!()
        };
        let controller = stay(on_paused(pausing, Ok(Paused(4))));
        assert!(matches!(
            controller,
            Controller::Paused(Live {
                machine: Paused(4),
                ..
            })
        ));
        assert_eq!(answer.try_recv(), Ok(Ok(())));
    }

    #[test]
    fn losing_every_client_ends_only_a_vmm_without_a_guest() {
        let (_, why) = exit(on_detached(Fake::NotStarted(MachineSpec::default())));
        assert_eq!(why, Exit::Detached);
        let (_pause, _, pausing) = mock_pause();
        let (_resume, _, resuming) = mock_resume();
        for controller in [
            Fake::Running(live(Running(1))),
            Fake::Paused(live(Paused(1))),
            pausing,
            resuming,
        ] {
            let state = controller.state();
            assert_eq!(stay(on_detached(controller)).state(), state);
        }
    }

    #[test]
    fn a_failed_start_leaves_the_spec_to_start_again() {
        // The build fails: answered at once.
        let (reply, answer) = make_reply();
        let controller: Fake = on_built(
            spec_with_kernel(),
            reply,
            Err(StartError::Plan(PlanError::NoKernel)),
        );
        assert_eq!(
            answer.try_recv(),
            Ok(Err(ApiError::msg(
                "plan the machine: no kernel is configured"
            )))
        );
        let Controller::NotStarted(spec) = controller else {
            panic!("a failed build left a machine");
        };
        assert_eq!(spec, spec_with_kernel());

        // The build succeeds: Start is answered once the machine has started.
        let (finish, operation) = operation();
        let (reply, answer) = make_reply();
        let controller: Fake = on_built(spec, reply, Ok((operation, events().1)));
        let (controller, state) = info(controller);
        assert_eq!(state, VmState::NotStarted);
        assert_eq!(answer.try_recv(), Err(oneshot::TryRecvError::Empty));

        // The first resume fails, and the machine is gone.
        let Controller::Busy(Busy::Starting(starting)) = controller else {
            panic!("a built machine did not start");
        };
        drop(finish);
        let error = StartError::Resume(ResumeError(VcpuError::Exited(0)));
        let controller = stay(on_started(starting, Err(error)));
        assert_eq!(
            answer.try_recv(),
            Ok(Err(ApiError::msg(
                "start the machine: resume the vCPUs: vCPU 0 has exited"
            )))
        );
        // Start again, with the same spec.
        let (request, _answer) = make_request(Start);
        match on_request(controller, request, INSTANCE) {
            Next::Start(spec, _) => assert_eq!(spec, spec_with_kernel()),
            _ => panic!("a failed start cannot be retried"),
        }

        // A start that succeeds runs the machine.
        let (_, answer, starting) = mock_start();
        let Controller::Busy(Busy::Starting(starting)) = starting else {
            unreachable!()
        };
        let controller = stay(on_started(starting, Ok(Running(5))));
        assert!(matches!(
            controller,
            Controller::Running(Live {
                machine: Running(5),
                ..
            })
        ));
        assert_eq!(answer.try_recv(), Ok(Ok(())));
    }

    #[test]
    fn a_start_that_fails_after_every_client_left_ends_the_vmm() {
        let (_, answer, starting) = mock_start();
        let Controller::Busy(Busy::Starting(starting)) = stay(on_detached(starting)) else {
            panic!("losing every client abandoned the start");
        };
        let error = StartError::Resume(ResumeError(VcpuError::TimedOut(0)));
        let (controller, why) = exit(on_started(starting, Err(error)));
        assert!(matches!(controller, Controller::NotStarted(_)));
        assert_eq!(why, Exit::Detached);
        assert!(answer.try_recv().unwrap().is_err());

        // One that succeeds keeps the guest running.
        let (_, _, starting) = mock_start();
        let Controller::Busy(Busy::Starting(starting)) = stay(on_detached(starting)) else {
            unreachable!()
        };
        let controller = stay(on_started(starting, Ok(Running(1))));
        assert!(matches!(controller, Controller::Running(_)));
    }

    #[test]
    fn teardown_errors_override_the_exit_reason() {
        for why in [Exit::Shutdown, Exit::Detached, Exit::GuestReset] {
            assert_eq!(exit_status(why.clone(), Ok(())), VmmExitStatus::Ok);
            assert_eq!(
                exit_status(why, Err(TeardownError::VmHeld)),
                VmmExitStatus::Error(
                    "tear down the machine: the VM is still held after teardown".into()
                )
            );
        }
        let failed = Exit::VcpuFailed {
            index: 0,
            error: "KVM_RUN failed".into(),
        };
        assert_eq!(
            exit_status(failed, Err(TeardownError::VmHeld)),
            VmmExitStatus::Error(
                "tear down the machine: the VM is still held after teardown".into()
            )
        );
    }
}
