use std::future::Future;
use std::pin::Pin;

use api::instance_info::{InstanceInfo, VmState};
use api::{ApiError, ApiResult, Reply, VmmCommand, VmmRequest};
use logger::{error_unlimited, info_unlimited, warn_unlimited};
use tokio::select;

use crate::devices::DeviceExit;
use crate::machine::MachineEvent;
use crate::machine::build::{Machine, build};
use crate::machine::plan::MachinePlan;
use crate::spec::MachineSpec;
use crate::{ProcessCtx, VmmExitStatus};

// ---------------------------------------------------------------------------
// Vm: the three logical states a virtual machine can be in.
// ---------------------------------------------------------------------------

pub(crate) enum Vm {
    NotStarted(MachineSpec),
    Running(Machine),
    Paused(Machine),
}

impl Vm {
    pub(crate) fn state(&self) -> VmState {
        match self {
            Self::NotStarted(_) => VmState::NotStarted,
            Self::Running(_) => VmState::Running,
            Self::Paused(_) => VmState::Paused,
        }
    }

    fn configure(self, command: VmmCommand) -> (Self, ApiResult<()>) {
        match self {
            Self::NotStarted(spec) => match spec.apply(command) {
                Ok(next) => (Self::NotStarted(next), Ok(())),
                Err(error) => (
                    Self::NotStarted(spec),
                    Err(ApiError::msg(format!("{error:#}"))),
                ),
            },
            vm => (
                vm,
                Err(ApiError::msg(
                    "machine configuration cannot change after Start",
                )),
            ),
        }
    }

    pub(crate) async fn teardown(self) -> anyhow::Result<()> {
        match self {
            Self::NotStarted(_) => Ok(()),
            Self::Running(machine) | Self::Paused(machine) => machine.teardown().await,
        }
    }

    pub(crate) async fn next_event(&mut self) -> Event {
        match self {
            Self::NotStarted(_) => std::future::pending().await,
            Self::Running(machine) | Self::Paused(machine) => select! {
                Some(event) = machine.events.recv() => Event::Machine(event),
                Some(exit) = machine.tasks.next_exit() => Event::Device(exit),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Operation: an in-flight lifecycle action that exclusively owns the Machine.
// ---------------------------------------------------------------------------

/// The result returned by an operation future when it completes.
///
/// By then the operation has already answered the request that started it.
pub(crate) struct OperationCompletion {
    /// The Vm the Controller goes back to, built by the operation itself.
    pub vm: Vm,
    /// What the main loop should do next.
    pub step: Step,
}

/// A lifecycle operation in progress. The future exclusively owns the Machine
/// and the reply channel of the request that started it.
pub(crate) struct Operation {
    /// The future performing the actual work. `'static + !Send`.
    pub future: Pin<Box<dyn Future<Output = OperationCompletion> + 'static>>,
    /// The sender half -- stored so Shutdown can trigger cancellation.
    pub cancel_tx: Option<oneshot::Sender<()>>,
}

// ---------------------------------------------------------------------------
// Controller: the top-level state machine.
// ---------------------------------------------------------------------------

#[allow(clippy::large_enum_variant)]
pub(crate) enum Controller {
    /// No lifecycle operation is running; the Vm owns the Machine.
    Idle(Vm),
    /// A lifecycle operation is in progress; the future owns the Machine.
    Busy {
        /// The state the Vm was in before the operation started.
        stable: VmState,
        /// The in-flight operation.
        operation: Operation,
    },
}

/// What the main loop should do after handling one event.
pub(crate) enum Step {
    Continue,
    Stop(VmmExitStatus),
}

impl Step {
    fn error(error: &anyhow::Error) -> Self {
        Self::Stop(VmmExitStatus::Error(format!("{error:#}")))
    }
}

/// Events the main loop selects on.
pub(crate) enum Event {
    Request(VmmRequest),
    /// Every client has gone away.
    Detached,
    Machine(MachineEvent),
    Device(DeviceExit),
}

fn warn_detached() {
    warn_unlimited!("all VMM clients are gone; control plane detached, VM continues as-is");
}

// ---------------------------------------------------------------------------
// Operation constructors for Pause, Resume, Start.
// ---------------------------------------------------------------------------

const INTERRUPTED: &str = "interrupted by shutdown";

fn make_pause_operation(mut machine: Machine, reply: Reply<ApiResult<()>>) -> (VmState, Operation) {
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    let future: Pin<Box<dyn Future<Output = OperationCompletion> + 'static>> =
        Box::pin(async move {
            let outcome = {
                let pause_fut = machine.pause();
                tokio::pin!(pause_fut);
                select! {
                    biased;
                    _ = cancel_rx => None,
                    result = &mut pause_fut => Some(result),
                }
            };
            match outcome {
                Some(Ok(())) => {
                    reply.send(Ok(()));
                    OperationCompletion {
                        vm: Vm::Paused(machine),
                        step: Step::Continue,
                    }
                }
                Some(Err(error)) => {
                    reply.send(Err(ApiError::msg(format!("{error:#}"))));
                    OperationCompletion {
                        vm: Vm::Running(machine),
                        step: Step::error(&error),
                    }
                }
                None => {
                    reply.send(Err(ApiError::msg(INTERRUPTED)));
                    OperationCompletion {
                        vm: Vm::Running(machine),
                        step: Step::Continue,
                    }
                }
            }
        });
    (
        VmState::Running,
        Operation {
            future,
            cancel_tx: Some(cancel_tx),
        },
    )
}

fn make_resume_operation(
    mut machine: Machine,
    reply: Reply<ApiResult<()>>,
) -> (VmState, Operation) {
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    let future: Pin<Box<dyn Future<Output = OperationCompletion> + 'static>> =
        Box::pin(async move {
            let outcome = {
                let resume_fut = machine.resume();
                tokio::pin!(resume_fut);
                select! {
                    biased;
                    _ = cancel_rx => None,
                    result = &mut resume_fut => Some(result),
                }
            };
            match outcome {
                Some(Ok(())) => {
                    reply.send(Ok(()));
                    OperationCompletion {
                        vm: Vm::Running(machine),
                        step: Step::Continue,
                    }
                }
                Some(Err(error)) => {
                    reply.send(Err(ApiError::msg(format!("{error:#}"))));
                    OperationCompletion {
                        vm: Vm::Paused(machine),
                        step: Step::error(&error),
                    }
                }
                None => {
                    reply.send(Err(ApiError::msg(INTERRUPTED)));
                    OperationCompletion {
                        vm: Vm::Paused(machine),
                        step: Step::Continue,
                    }
                }
            }
        });
    (
        VmState::Paused,
        Operation {
            future,
            cancel_tx: Some(cancel_tx),
        },
    )
}

fn make_start_operation(
    spec: MachineSpec,
    machine: Machine,
    reply: Reply<ApiResult<()>>,
) -> (VmState, Operation) {
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    let future: Pin<Box<dyn Future<Output = OperationCompletion> + 'static>> =
        Box::pin(async move {
            let outcome = {
                let resume_fut = machine.vcpus.as_ref().unwrap().resume();
                tokio::pin!(resume_fut);
                select! {
                    biased;
                    _ = cancel_rx => None,
                    result = &mut resume_fut => Some(result),
                }
            };
            match outcome {
                Some(Ok(())) => {
                    reply.send(Ok(()));
                    OperationCompletion {
                        vm: Vm::Running(machine),
                        step: Step::Continue,
                    }
                }
                Some(Err(error)) => {
                    if let Err(teardown_error) = machine.teardown().await {
                        error_unlimited!(
                            "tear down machine after failed start: {teardown_error:#}"
                        );
                    }
                    reply.send(Err(ApiError::msg(format!("start vCPUs: {error:#}"))));
                    OperationCompletion {
                        vm: Vm::NotStarted(spec),
                        step: Step::Continue,
                    }
                }
                None => {
                    reply.send(Err(ApiError::msg(INTERRUPTED)));
                    OperationCompletion {
                        // The variant only selects teardown; the vCPUs may never have run.
                        vm: Vm::Running(machine),
                        step: Step::Continue,
                    }
                }
            }
        });
    (
        VmState::NotStarted,
        Operation {
            future,
            cancel_tx: Some(cancel_tx),
        },
    )
}

// ---------------------------------------------------------------------------
// Controller implementation.
// ---------------------------------------------------------------------------

impl Controller {
    pub(crate) fn new(spec: MachineSpec) -> Self {
        Controller::Idle(Vm::NotStarted(spec))
    }

    /// Teardown the VM, regardless of state.
    pub(crate) async fn teardown(self) -> anyhow::Result<()> {
        match self {
            Controller::Idle(vm) => vm.teardown().await,
            Controller::Busy {
                mut operation,
                stable: _,
            } => {
                // Cancel the operation and await it to get the Vm back.
                if let Some(cancel_tx) = operation.cancel_tx.take() {
                    let _ = cancel_tx.send(());
                }
                operation.future.await.vm.teardown().await
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Idle-state handler.
// ---------------------------------------------------------------------------

pub(crate) async fn handle_idle(vm: Vm, event: Event, ctx: &ProcessCtx) -> (Controller, Step) {
    let request = match event {
        Event::Request(request) => request,
        Event::Detached => {
            let step = match vm {
                // No guest to protect, so the vmm thread need not outlive its clients.
                Vm::NotStarted(_) => Step::Stop(VmmExitStatus::Ok),
                Vm::Running(_) | Vm::Paused(_) => {
                    warn_detached();
                    Step::Continue
                }
            };
            return (Controller::Idle(vm), step);
        }
        Event::Machine(MachineEvent::GuestReset) => {
            return (Controller::Idle(vm), Step::Stop(VmmExitStatus::Ok));
        }
        Event::Machine(MachineEvent::VcpuFailed { index, error }) => {
            return (
                Controller::Idle(vm),
                Step::Stop(VmmExitStatus::Error(format!("vCPU {index}: {error}"))),
            );
        }
        Event::Device(exit) => {
            match exit.result {
                Ok(()) => info_unlimited!("device {} stopped", exit.name),
                Err(error) => warn_unlimited!("device {} failed: {error:#}", exit.name),
            }
            return (Controller::Idle(vm), Step::Continue);
        }
    };
    match request {
        VmmRequest::GetInstanceInfo(_, reply) => {
            reply.send(InstanceInfo {
                id: ctx.id.clone(),
                version: ctx.version.clone(),
                state: vm.state(),
            });
            (Controller::Idle(vm), Step::Continue)
        }
        VmmRequest::UpdateLogger(cfg, reply) => {
            reply.send((ctx.update_logger)(cfg.0));
            (Controller::Idle(vm), Step::Continue)
        }
        VmmRequest::HandoverState(_, reply) => {
            reply.send(Err(ApiError::msg("live upgrade is not supported")));
            (Controller::Idle(vm), Step::Continue)
        }
        VmmRequest::Shutdown(_, reply) => {
            reply.send(Ok(()));
            (Controller::Idle(vm), Step::Stop(VmmExitStatus::Ok))
        }
        VmmRequest::SetCpus(value, reply) => {
            let (next, result) = vm.configure(VmmCommand::SetCpus(value, ()));
            reply.send(result);
            (Controller::Idle(next), Step::Continue)
        }
        VmmRequest::SetMemory(value, reply) => {
            let (next, result) = vm.configure(VmmCommand::SetMemory(value, ()));
            reply.send(result);
            (Controller::Idle(next), Step::Continue)
        }
        VmmRequest::SetMemoryBacking(value, reply) => {
            let (next, result) = vm.configure(VmmCommand::SetMemoryBacking(value, ()));
            reply.send(result);
            (Controller::Idle(next), Step::Continue)
        }
        VmmRequest::SetConsole(value, reply) => {
            let (next, result) = vm.configure(VmmCommand::SetConsole(value, ()));
            reply.send(result);
            (Controller::Idle(next), Step::Continue)
        }
        VmmRequest::SetKernel(value, reply) => {
            let (next, result) = vm.configure(VmmCommand::SetKernel(value, ()));
            reply.send(result);
            (Controller::Idle(next), Step::Continue)
        }
        VmmRequest::AddDisk(value, reply) => {
            let (next, result) = vm.configure(VmmCommand::AddDisk(value, ()));
            reply.send(result);
            (Controller::Idle(next), Step::Continue)
        }
        VmmRequest::Start(_, reply) => match vm {
            Vm::NotStarted(spec) => {
                let result = MachinePlan::new(spec.clone(), ctx.kvm_ctx.capabilities())
                    .and_then(|plan| build(plan, &ctx.kvm_ctx));
                match result {
                    Ok(machine) => {
                        let (stable, operation) = make_start_operation(spec, machine, reply);
                        (Controller::Busy { stable, operation }, Step::Continue)
                    }
                    Err(error) => {
                        reply.send(Err(ApiError::msg(format!("{error:#}"))));
                        (Controller::Idle(Vm::NotStarted(spec)), Step::Continue)
                    }
                }
            }
            vm => {
                reply.send(Err(ApiError::msg("VM is already started")));
                (Controller::Idle(vm), Step::Continue)
            }
        },
        VmmRequest::Pause(_, reply) => match vm {
            Vm::Running(machine) => {
                let (stable, operation) = make_pause_operation(machine, reply);
                (Controller::Busy { stable, operation }, Step::Continue)
            }
            Vm::Paused(machine) => {
                reply.send(Ok(()));
                (Controller::Idle(Vm::Paused(machine)), Step::Continue)
            }
            vm => {
                reply.send(Err(ApiError::msg("VM is not started")));
                (Controller::Idle(vm), Step::Continue)
            }
        },
        VmmRequest::Resume(_, reply) => match vm {
            Vm::Paused(machine) => {
                let (stable, operation) = make_resume_operation(machine, reply);
                (Controller::Busy { stable, operation }, Step::Continue)
            }
            Vm::Running(machine) => {
                reply.send(Ok(()));
                (Controller::Idle(Vm::Running(machine)), Step::Continue)
            }
            vm => {
                reply.send(Err(ApiError::msg("VM is not started")));
                (Controller::Idle(vm), Step::Continue)
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Busy-state handler: only handles requests while an operation is in flight.
// ---------------------------------------------------------------------------

/// Result of handling a request while Busy.
pub(crate) enum BusyAction {
    /// Keep going; operation is unchanged.
    Continue(Operation),
    /// Shutdown requested during Busy. Cancel sent, break the request loop.
    Shutdown(Operation),
}

/// What the main loop observed while Busy.
pub(crate) enum BusyEvent {
    Completion(Box<OperationCompletion>),
    Request(VmmRequest),
    /// Every client has gone away.
    Detached,
}

/// Subset of [`ProcessCtx`] fields that the busy handler needs. Avoids
/// requiring a full ProcessCtx (with KvmContext) for unit testing.
pub(crate) struct BusyCtx<'a> {
    pub id: &'a str,
    pub version: &'a str,
    pub update_logger: fn(api::logger::LoggerConfig) -> ApiResult<()>,
}

impl<'a> From<&'a ProcessCtx> for BusyCtx<'a> {
    fn from(ctx: &'a ProcessCtx) -> Self {
        Self {
            id: &ctx.id,
            version: &ctx.version,
            update_logger: ctx.update_logger,
        }
    }
}

pub(crate) fn handle_busy_request(
    stable: VmState,
    mut operation: Operation,
    request: VmmRequest,
    ctx: &BusyCtx<'_>,
) -> BusyAction {
    match request {
        VmmRequest::GetInstanceInfo(_, reply) => {
            reply.send(InstanceInfo {
                id: ctx.id.to_owned(),
                version: ctx.version.to_owned(),
                state: stable.clone(),
            });
            BusyAction::Continue(operation)
        }
        VmmRequest::UpdateLogger(cfg, reply) => {
            reply.send((ctx.update_logger)(cfg.0));
            BusyAction::Continue(operation)
        }
        VmmRequest::HandoverState(_, reply) => {
            reply.send(Err(ApiError::msg("live upgrade is not supported")));
            BusyAction::Continue(operation)
        }
        VmmRequest::Shutdown(_, reply) => {
            reply.send(Ok(()));
            if let Some(cancel_tx) = operation.cancel_tx.take() {
                let _ = cancel_tx.send(());
            }
            BusyAction::Shutdown(operation)
        }
        other => {
            reject_busy_request(other);
            BusyAction::Continue(operation)
        }
    }
}

fn reject_busy_request(request: VmmRequest) {
    let error = ApiError::msg("lifecycle operation in progress");
    match request {
        VmmRequest::SetCpus(_, reply) => reply.send(Err(error)),
        VmmRequest::SetMemory(_, reply) => reply.send(Err(error)),
        VmmRequest::SetMemoryBacking(_, reply) => reply.send(Err(error)),
        VmmRequest::SetConsole(_, reply) => reply.send(Err(error)),
        VmmRequest::SetKernel(_, reply) => reply.send(Err(error)),
        VmmRequest::AddDisk(_, reply) => reply.send(Err(error)),
        VmmRequest::Start(_, reply) => reply.send(Err(error)),
        VmmRequest::Pause(_, reply) => reply.send(Err(error)),
        VmmRequest::Resume(_, reply) => reply.send(Err(error)),
        VmmRequest::GetInstanceInfo(_, _)
        | VmmRequest::UpdateLogger(_, _)
        | VmmRequest::HandoverState(_, _)
        | VmmRequest::Shutdown(_, _) => {}
    }
}

/// Complete an operation: transition back to Idle with the Vm it produced.
pub(crate) fn complete_operation(completion: OperationCompletion) -> (Controller, Step) {
    (Controller::Idle(completion.vm), completion.step)
}

/// Handle one event while the Controller is Busy.
pub(crate) async fn handle_busy(
    stable: VmState,
    operation: Operation,
    event: BusyEvent,
    ctx: &BusyCtx<'_>,
) -> (Controller, Step) {
    match event {
        BusyEvent::Completion(completion) => complete_operation(*completion),
        BusyEvent::Detached => {
            warn_detached();
            (Controller::Busy { stable, operation }, Step::Continue)
        }
        BusyEvent::Request(request) => {
            match handle_busy_request(stable.clone(), operation, request, ctx) {
                BusyAction::Continue(op) => (
                    Controller::Busy {
                        stable,
                        operation: op,
                    },
                    Step::Continue,
                ),
                BusyAction::Shutdown(op) => {
                    let completion = op.future.await;
                    (
                        Controller::Idle(completion.vm),
                        Step::Stop(VmmExitStatus::Ok),
                    )
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::Path;

    use api::RawVmmOp;
    use api::instance_info::GetInstanceInfo;
    use api::lifecycle::{Pause, Shutdown};
    use api::machine::{MemorySizeMib, SetMemory};

    use super::*;
    use crate::KvmContext;

    #[test]
    fn shutdown_reply_means_request_accepted() {
        let (request, receiver) = Shutdown.into_vmm_request();
        let VmmRequest::Shutdown(_, reply) = request else {
            panic!("expected Shutdown request");
        };

        reply.send(Ok(()));
        assert_eq!(receiver.try_recv(), Ok(Ok(())));
    }

    // ------------------------------------------------------------------
    // Controller unit tests using controllable futures (no guest required;
    // idle handling takes a whole ProcessCtx, which opens /dev/kvm).
    // ------------------------------------------------------------------

    fn test_busy_ctx() -> BusyCtx<'static> {
        BusyCtx {
            id: "test-id",
            version: "test-version",
            update_logger: |_| Ok(()),
        }
    }

    fn test_process_ctx() -> Option<ProcessCtx> {
        if !Path::new("/dev/kvm").exists() {
            return None;
        }
        Some(ProcessCtx {
            id: "test-id".into(),
            version: "test-version".into(),
            kvm_ctx: KvmContext::from_dev_path("/dev/kvm").unwrap(),
            update_logger: |_| Ok(()),
        })
    }

    /// Create a pending operation with a controllable completion channel, and
    /// the receiver that observes whether it was cancelled.
    fn mock_operation() -> (
        Operation,
        oneshot::Sender<OperationCompletion>,
        oneshot::Receiver<()>,
    ) {
        let (completion_tx, completion_rx) = oneshot::channel();
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        let future: Pin<Box<dyn Future<Output = OperationCompletion> + 'static>> =
            Box::pin(async move { completion_rx.await.unwrap() });
        let operation = Operation {
            future,
            cancel_tx: Some(cancel_tx),
        };
        (operation, completion_tx, cancel_rx)
    }

    /// What a Start whose vCPUs failed to resume completes with.
    fn not_started() -> OperationCompletion {
        OperationCompletion {
            vm: Vm::NotStarted(MachineSpec::default()),
            step: Step::Continue,
        }
    }

    #[tokio::test(flavor = "local")]
    async fn busy_get_instance_info_returns_stable_state() {
        let (operation, _completion_tx, _cancel_rx) = mock_operation();
        let (request, receiver) = GetInstanceInfo.into_vmm_request();
        let ctx = test_busy_ctx();

        let action = handle_busy_request(VmState::Running, operation, request, &ctx);
        assert!(matches!(action, BusyAction::Continue(_)));

        let info = receiver.try_recv().unwrap();
        assert_eq!(info.state, VmState::Running);
        assert_eq!(info.id, "test-id");
    }

    #[tokio::test(flavor = "local")]
    async fn busy_config_request_rejected() {
        let (operation, _completion_tx, _cancel_rx) = mock_operation();
        let (request, receiver) = SetMemory(MemorySizeMib(32)).into_vmm_request();
        let ctx = test_busy_ctx();

        let action = handle_busy_request(VmState::Running, operation, request, &ctx);
        assert!(matches!(action, BusyAction::Continue(_)));

        let result = receiver.try_recv().unwrap();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("lifecycle operation in progress")
        );
    }

    #[tokio::test(flavor = "local")]
    async fn busy_lifecycle_request_rejected() {
        let (operation, _completion_tx, _cancel_rx) = mock_operation();
        let (request, receiver) = Pause.into_vmm_request();
        let ctx = test_busy_ctx();

        let action = handle_busy_request(VmState::Running, operation, request, &ctx);
        assert!(matches!(action, BusyAction::Continue(_)));

        let result = receiver.try_recv().unwrap();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("lifecycle operation in progress")
        );
    }

    #[tokio::test(flavor = "local")]
    async fn busy_shutdown_replies_ok_and_signals_cancel() {
        let (operation, _completion_tx, cancel_rx) = mock_operation();
        let (request, receiver) = Shutdown.into_vmm_request();
        let ctx = test_busy_ctx();

        let action = handle_busy_request(VmState::Running, operation, request, &ctx);
        assert!(matches!(action, BusyAction::Shutdown(_)));

        let result = receiver.try_recv().unwrap();
        assert_eq!(result, Ok(()));
        assert_eq!(cancel_rx.try_recv(), Ok(()));
    }

    #[tokio::test(flavor = "local")]
    async fn client_disconnect_during_busy_lets_the_operation_complete() {
        let (operation, completion_tx, cancel_rx) = mock_operation();
        let ctx = test_busy_ctx();

        let (controller, step) =
            handle_busy(VmState::Running, operation, BusyEvent::Detached, &ctx).await;
        assert!(matches!(step, Step::Continue));
        let Controller::Busy {
            stable,
            mut operation,
        } = controller
        else {
            panic!("losing every client abandoned the operation");
        };
        assert_eq!(cancel_rx.try_recv(), Err(oneshot::TryRecvError::Empty));

        completion_tx.send(not_started()).unwrap();
        let completion = (&mut operation.future).await;
        let (controller, step) = handle_busy(
            stable,
            operation,
            BusyEvent::Completion(Box::new(completion)),
            &ctx,
        )
        .await;
        assert!(matches!(step, Step::Continue));
        assert!(matches!(controller, Controller::Idle(Vm::NotStarted(_))));
    }

    #[tokio::test(flavor = "local")]
    async fn failed_start_completion_leaves_an_idle_not_started_vm() {
        let Some(ctx) = test_process_ctx() else {
            return;
        };
        let (operation, _completion_tx, _cancel_rx) = mock_operation();

        let (controller, step) = handle_busy(
            VmState::NotStarted,
            operation,
            BusyEvent::Completion(Box::new(not_started())),
            &BusyCtx::from(&ctx),
        )
        .await;
        assert!(matches!(step, Step::Continue));
        let Controller::Idle(vm) = controller else {
            panic!("controller stayed busy after the operation completed");
        };

        let (request, receiver) = GetInstanceInfo.into_vmm_request();
        let (controller, step) = handle_idle(vm, Event::Request(request), &ctx).await;
        assert!(matches!(step, Step::Continue));
        assert!(matches!(controller, Controller::Idle(Vm::NotStarted(_))));
        assert_eq!(receiver.try_recv().unwrap().state, VmState::NotStarted);
    }

    #[tokio::test(flavor = "local")]
    async fn client_disconnect_before_start_stops_the_vmm() {
        let Some(ctx) = test_process_ctx() else {
            return;
        };

        let (controller, step) = handle_idle(
            Vm::NotStarted(MachineSpec::default()),
            Event::Detached,
            &ctx,
        )
        .await;
        assert!(matches!(step, Step::Stop(VmmExitStatus::Ok)));
        assert!(matches!(controller, Controller::Idle(Vm::NotStarted(_))));
    }
}
