//! Lifecycle decisions live in `controller`; this loop owns requests, events
//! and one local operation future at a time. Selecting only borrows that
//! future: losing a select round never drops or cancels the operation.

mod controller;

use std::future::pending;

use api::{ApiResult, Reply};
use logger::warn_unlimited;
use tokio::select;

use self::controller::{
    Busy, Controller, Event, Exit, Instance, Live, Next, StartError, Transition,
};
use crate::VmmExitStatus;
use crate::error::render_chain;
use crate::machine::{
    FailedMachine, Machine, MachineEvents, MachinePlan, PauseError, PausedMachine, ResumeError,
    TeardownError, build,
};
use crate::process_ctx::ProcessCtx;
use crate::spec::MachineSpec;
use crate::vmm_server::{Inbound, VmmServer};

type State = Controller<Machine, PausedMachine, FailedMachine>;

/// Completion variants carry exactly the machine type their operation returns.
enum Input {
    Inbound(Inbound),
    Event(Event),
    Started(Result<Machine, StartError>),
    Paused(Result<PausedMachine, (FailedMachine, PauseError)>),
    Resumed(Result<Machine, (FailedMachine, ResumeError)>),
}

pub(crate) async fn run(server: VmmServer, ctx: &ProcessCtx) -> VmmExitStatus {
    serve(server, ctx, State::NotStarted(MachineSpec::default())).await
}

async fn serve(server: VmmServer, ctx: &ProcessCtx, mut controller: State) -> VmmExitStatus {
    let instance = Instance {
        id: &ctx.id,
        version: &ctx.version,
        update_logger: ctx.update_logger,
    };
    let mut server = Some(server);
    let mut stopping = None;
    loop {
        // On exit, an operation still owns its machine until it finishes.
        // Keep selecting events meanwhile, but never take another request.
        if let Some(exit) = stopping.take() {
            match &controller {
                Controller::Busy(_) => stopping = Some(exit),
                Controller::NotStarted(_) | Controller::Running(_) | Controller::Paused(_) => {
                    return teardown(controller, exit).await;
                }
            }
        }
        let input = next_input(&mut controller, &mut server).await;
        // The operation can enqueue an event during its final poll, AFTER
        // the event arm was polled. Consume it before dropping Start's events
        // or moving straight from completion to teardown.
        match &input {
            Input::Started(_) | Input::Paused(_) | Input::Resumed(_) => {
                let events = match &mut controller {
                    Controller::Busy(Busy::Starting(starting)) => &mut starting.events,
                    Controller::Busy(Busy::Pausing(pausing)) => &mut pausing.events,
                    Controller::Busy(Busy::Resuming(resuming)) => &mut resuming.events,
                    Controller::NotStarted(_) | Controller::Running(_) | Controller::Paused(_) => {
                        unreachable!("completion while not Busy")
                    }
                };
                while let Ok(event) = events.machine.try_recv() {
                    let exit = machine_exit(event);
                    stopping = Some(prefer_exit(stopping.take(), exit));
                    server = None;
                }
            }
            Input::Inbound(_) | Input::Event(_) => {}
        }
        // Start remains retryable after successful cleanup. If already exiting,
        // however, a reset must not hide a CPU timeout/panic/join failure.
        if let Input::Started(Err(StartError::Resume(error))) = &input
            && stopping.is_some()
        {
            stopping = Some(prefer_exit(
                stopping.take(),
                controller::transition_failure(error, &error.0),
            ));
        }
        let next = match (controller, input) {
            (state, Input::Inbound(inbound)) => controller::on_inbound(state, inbound, instance),
            (state, Input::Event(event)) => controller::on_event(state, event),
            (Controller::Busy(Busy::Starting(starting)), Input::Started(result)) => {
                controller::on_started(starting, result)
            }
            (Controller::Busy(Busy::Pausing(pausing)), Input::Paused(result)) => {
                controller::on_paused(pausing, result)
            }
            (Controller::Busy(Busy::Resuming(resuming)), Input::Resumed(result)) => {
                controller::on_resumed(resuming, result)
            }
            // next_input only polls the operation belonging to this state.
            (
                Controller::NotStarted(_) | Controller::Running(_) | Controller::Paused(_),
                Input::Started(_) | Input::Paused(_) | Input::Resumed(_),
            )
            | (Controller::Busy(Busy::Starting(_)), Input::Paused(_) | Input::Resumed(_))
            | (Controller::Busy(Busy::Pausing(_)), Input::Started(_) | Input::Resumed(_))
            | (Controller::Busy(Busy::Resuming(_)), Input::Started(_) | Input::Paused(_)) => {
                unreachable!("completion from a different operation")
            }
        };
        controller = match next {
            Next::Stay(state) => state,
            Next::Start(spec, reply) => start(spec, reply, ctx),
            Next::Pause(running, reply) => {
                Controller::Busy(Busy::Pausing(Transition::new(running, reply, |machine| {
                    Box::pin(machine.pause())
                })))
            }
            Next::Resume(paused, reply) => {
                Controller::Busy(Busy::Resuming(Transition::new(paused, reply, |machine| {
                    Box::pin(machine.resume())
                })))
            }
            Next::Terminate(Live { machine, events }, exit) => {
                // A failed transition cannot enter any serving state.
                // Close admission before polling its teardown, retaining events.
                drop(server.take());
                let exit = prefer_exit(stopping.take(), exit);
                return poll_teardown(
                    Box::pin(async move { machine.teardown().await.map(drop) }),
                    events,
                    exit,
                )
                .await;
            }
            Next::Exit(state, exit) => {
                // Dropping the receiver closes admission and drops queued replies.
                server = None;
                // A fatal event while stopping must not be hidden by Shutdown/reset.
                stopping = Some(prefer_exit(stopping.take(), exit));
                state
            }
        };
    }
}

fn start(spec: MachineSpec, reply: Reply<ApiResult<()>>, ctx: &ProcessCtx) -> State {
    // The documented synchronous exception on vmm-main: reading kernel/initrd
    // and building the parked machine. It has not run any guest code yet.
    let built = MachinePlan::new(&spec, ctx.kvm_ctx.capabilities())
        .map_err(StartError::Plan)
        .and_then(|(plan, policy)| build(plan, policy, &ctx.kvm_ctx).map_err(StartError::Build));
    controller::on_built(
        spec,
        reply,
        built.map(|(paused, events)| {
            let operation: controller::Operation<Result<Machine, StartError>> =
                Box::pin(async move {
                    match paused.resume().await {
                        Ok(running) => Ok(running),
                        Err((failed, error)) => {
                            // A failed first resume is retryable, but only after
                            // tearing down its sole failure owner. Never cancel this.
                            warn_unlimited!("failed Start: {}", render_chain(&error));
                            failed.teardown().await.map_err(StartError::Teardown)?;
                            Err(StartError::Resume(error))
                        }
                    }
                });
            (operation, events)
        }),
    )
}

/// Events precede completions and requests. In particular a reset is not
/// reclassified as a fatal PauseError::Exited if both are ready together.
async fn next_input(state: &mut State, server: &mut Option<VmmServer>) -> Input {
    match state {
        Controller::NotStarted(_) => Input::Inbound(next_inbound(server).await),
        Controller::Running(Live { events, .. }) | Controller::Paused(Live { events, .. }) => {
            select! {
                biased;
                event = next_event(events) => Input::Event(event),
                inbound = next_inbound(server) => Input::Inbound(inbound),
            }
        }
        Controller::Busy(Busy::Starting(starting)) => {
            select! {
                biased;
                event = next_event(&mut starting.events) => Input::Event(event),
                result = &mut starting.operation => Input::Started(result),
                inbound = next_inbound(server) => Input::Inbound(inbound),
            }
        }
        Controller::Busy(Busy::Pausing(pausing)) => {
            select! {
                biased;
                event = next_event(&mut pausing.events) => Input::Event(event),
                result = &mut pausing.operation => Input::Paused(result),
                inbound = next_inbound(server) => Input::Inbound(inbound),
            }
        }
        Controller::Busy(Busy::Resuming(resuming)) => {
            select! {
                biased;
                event = next_event(&mut resuming.events) => Input::Event(event),
                result = &mut resuming.operation => Input::Resumed(result),
                inbound = next_inbound(server) => Input::Inbound(inbound),
            }
        }
    }
}

async fn next_inbound(server: &mut Option<VmmServer>) -> Inbound {
    match server {
        Some(server) => server.next_inbound().await,
        None => pending().await,
    }
}

/// Closed streams do not spin or hide the other source. Both recv methods
/// are cancel-safe; an unready event future can be dropped each select round.
async fn next_event(events: &mut MachineEvents) -> Event {
    select! {
        biased;
        Some(event) = events.machine.recv() => Event::Machine(event),
        Some(exit) = events.devices.next() => Event::Device(exit),
        else => pending().await,
    }
}

fn prefer_exit(prior: Option<Exit>, next: Exit) -> Exit {
    match (prior, next) {
        (_, teardown @ Exit::StartTeardown(_)) => teardown,
        (Some(prior @ Exit::StartTeardown(_)), _) => prior,
        (_, fatal @ Exit::VcpuFailed { .. }) => fatal,
        (Some(prior @ Exit::VcpuFailed { .. }), _) => prior,
        (_, failure @ Exit::TransitionFailed(_)) => failure,
        (Some(prior @ Exit::TransitionFailed(_)), _) => prior,
        (Some(reset @ Exit::GuestReset), Exit::TransitionExited(_)) => reset,
        (Some(Exit::TransitionExited(_)), reset @ Exit::GuestReset) => reset,
        (_, fallback @ Exit::TransitionExited(_)) => fallback,
        (Some(prior), _) => prior,
        (None, exit) => exit,
    }
}

fn machine_exit(event: crate::machine::MachineEvent) -> Exit {
    match event {
        crate::machine::MachineEvent::GuestReset => Exit::GuestReset,
        crate::machine::MachineEvent::VcpuFailed { index, error } => {
            Exit::VcpuFailed { index, error }
        }
    }
}

async fn teardown(state: State, exit: Exit) -> VmmExitStatus {
    // Busy is settled by the serving loop, which keeps observing events.
    let (operation, events): (controller::Operation<_>, _) = match state {
        Controller::NotStarted(_) => return controller::exit_status(exit, Ok(())),
        Controller::Running(Live { machine, events }) => (
            Box::pin(async move { machine.teardown().await.map(drop) }),
            events,
        ),
        Controller::Paused(Live { machine, events }) => (
            Box::pin(async move { machine.teardown().await.map(drop) }),
            events,
        ),
        Controller::Busy(_) => unreachable!("teardown before the operation finished"),
    };
    poll_teardown(operation, events, exit).await
}

/// Shared by live and failed owners; the operation and event streams survive
/// every select round, including shutdown and the final-poll event drain.
async fn poll_teardown(
    mut operation: controller::Operation<Result<(), TeardownError>>,
    mut events: MachineEvents,
    mut exit: Exit,
) -> VmmExitStatus {
    // Teardown is also never cancelled. Events retain their meaning until it
    // completes; a late vCPU failure must not disappear behind Shutdown.
    let result = loop {
        select! {
            biased;
            event = next_event(&mut events) => match event {
                Event::Machine(event) => exit = prefer_exit(Some(exit), machine_exit(event)),
                Event::Device(device) => warn_unlimited!("device {} exited during teardown", device.name),
            },
            result = &mut operation => break result,
        }
    };
    // Same final-poll race as a lifecycle completion above.
    while let Ok(event) = events.machine.try_recv() {
        exit = prefer_exit(Some(exit), machine_exit(event));
    }
    controller::exit_status(exit, result)
}

#[cfg(test)]
mod tests {

    use std::path::Path;
    use std::time::Duration;

    use api::RawVmmOp;
    use api::boot::{KernelConfig, SetKernel};
    use api::lifecycle::Pause;
    use api::machine::{ConsoleConfig, SetConsole, SetCpus, VcpuCount};
    use tokio::sync::oneshot;
    use vm_memory::{Bytes, GuestAddress};

    use super::*;
    use crate::{KvmContext, test_guest as elf};

    #[tokio::test(flavor = "local")]
    async fn cpu_failure_precedence_distinguishes_exited_from_fatal_errors() {
        use crate::machine::VcpuError;

        let join = tokio::spawn(async { panic!("join failure") })
            .await
            .unwrap_err();
        for vcpu in [
            VcpuError::Exited(0),
            VcpuError::TimedOut(0),
            VcpuError::Panicked(0),
            VcpuError::Join {
                index: 0,
                source: join,
            },
        ] {
            let exited = matches!(vcpu, VcpuError::Exited(_));
            let error = ResumeError(vcpu);
            let failure = controller::transition_failure(&error, &error.0);
            for merged in [
                prefer_exit(Some(Exit::GuestReset), failure.clone()),
                prefer_exit(Some(failure.clone()), Exit::GuestReset),
            ] {
                assert_eq!(
                    controller::exit_status(merged.clone(), Ok(())) == VmmExitStatus::Ok,
                    exited
                );
                assert_eq!(
                    controller::exit_status(merged, Err(TeardownError::VmHeld)),
                    VmmExitStatus::Error(
                        "tear down the machine: the VM is still held after teardown".into()
                    ),
                );
            }
            assert!(matches!(
                controller::exit_status(prefer_exit(Some(Exit::Shutdown), failure), Ok(())),
                VmmExitStatus::Error(_),
            ));
        }
    }

    #[tokio::test(flavor = "local")]
    async fn shutdown_and_events_preserve_a_failed_owner_through_cleanup() {
        use std::cell::Cell;
        use std::future::poll_fn;
        use std::pin::pin;
        use std::rc::Rc;
        use std::task::Poll;

        struct FakePaused(Rc<Cell<usize>>, Rc<Cell<usize>>);
        struct FakeFailed(Rc<Cell<usize>>);
        impl Drop for FakeFailed {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        type Fake = Controller<(), FakePaused, FakeFailed>;
        let drops = Rc::new(Cell::new(0));
        let commands = Rc::new(Cell::new(0));
        let (sender, machine) = tokio::sync::mpsc::unbounded_channel();
        let (mut runtime, devices) = crate::devices::runtime::DeviceRuntime::new();
        let (device_exit, device_wait) = oneshot::channel();
        let device = runtime
            .spawn::<(), _, _, std::io::Error>(
                "cleanup-local-failure",
                crate::devices::runtime::Start::Running,
                |_| async move {
                    device_wait.await.unwrap();
                    Err(std::io::Error::other("local fault"))
                },
            )
            .unwrap();
        let (request, answer) = api::lifecycle::Resume.into_vmm_request();
        let api::VmmRequest::Resume(_, reply) = request else {
            unreachable!()
        };
        let (ack, unanswered) = oneshot::channel::<()>();
        let transition = Transition::new(
            Live {
                machine: FakePaused(drops.clone(), commands.clone()),
                events: MachineEvents { machine, devices },
            },
            reply,
            |FakePaused(drops, commands)| {
                Box::pin(async move {
                    commands.set(commands.get() + 1);
                    assert!(
                        tokio::time::timeout(Duration::from_millis(10), unanswered)
                            .await
                            .is_err()
                    );
                    Err((
                        FakeFailed(drops),
                        ResumeError(crate::machine::VcpuError::TimedOut(0)),
                    ))
                })
            },
        );
        let state: Fake = Controller::Busy(Busy::Resuming(transition));
        let (shutdown, shutdown_answer) = api::lifecycle::Shutdown.into_vmm_request();
        let instance = Instance {
            id: "fake",
            version: "test",
            update_logger: |_| Ok(()),
        };
        let Next::Exit(state, shutdown) = controller::on_request(state, shutdown, instance) else {
            panic!()
        };
        assert_eq!(shutdown_answer.try_recv(), Ok(Ok(())));
        let Next::Exit(state, reset) = controller::on_event(
            state,
            Event::Machine(crate::machine::MachineEvent::GuestReset),
        ) else {
            panic!()
        };
        let Controller::Busy(Busy::Resuming(mut resuming)) = state else {
            panic!()
        };
        assert!(poll_fn(|cx| Poll::Ready(resuming.operation.as_mut().poll(cx).is_pending())).await);
        assert_eq!(drops.get(), 0);
        assert_eq!(answer.try_recv(), Err(::oneshot::TryRecvError::Empty));
        let result = (&mut resuming.operation).await;
        drop(ack);
        let next: controller::Next<(), FakePaused, FakeFailed> =
            controller::on_resumed(resuming, result);
        let Next::Terminate(
            Live {
                machine: failed,
                events,
            },
            failure,
        ) = next
        else {
            panic!("failed resume restored a paused wrapper")
        };
        assert_eq!(commands.get(), 1, "a compensating CPU command was issued");
        assert!(answer.try_recv().unwrap().is_err());
        let (release, cleanup_wait) = oneshot::channel();
        let started = Rc::new(Cell::new(0));
        let completed = Rc::new(Cell::new(0));
        let operation = {
            let started = started.clone();
            let completed = completed.clone();
            let sender = sender.clone();
            Box::pin(async move {
                started.set(started.get() + 1);
                cleanup_wait.await.unwrap();
                sender
                    .send(crate::machine::MachineEvent::GuestReset)
                    .unwrap();
                drop(failed);
                completed.set(completed.get() + 1);
                Ok(())
            })
        };
        let exit = prefer_exit(Some(prefer_exit(Some(shutdown), reset)), failure);
        let mut cleanup = pin!(poll_teardown(operation, events, exit));
        assert!(poll_fn(|cx| Poll::Ready(cleanup.as_mut().poll(cx).is_pending())).await);
        device_exit.send(()).unwrap();
        device.finished().await;
        sender
            .send(crate::machine::MachineEvent::GuestReset)
            .unwrap();
        assert!(poll_fn(|cx| Poll::Ready(cleanup.as_mut().poll(cx).is_pending())).await);
        assert_eq!((started.get(), completed.get(), drops.get()), (1, 0, 0));
        release.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), cleanup)
                .await
                .unwrap(),
            VmmExitStatus::Error("resume the vCPUs: vCPU 0 did not answer in time".into())
        );
        assert_eq!((started.get(), completed.get(), drops.get()), (1, 1, 1));
        runtime.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(flavor = "local")]
    async fn events_queued_in_a_failed_starts_final_poll_are_not_lost() {
        if !Path::new("/dev/kvm").exists() {
            return;
        }
        let ctx = ProcessCtx {
            id: "final-poll".into(),
            version: "test".into(),
            kvm_ctx: KvmContext::from_dev_path("/dev/kvm").unwrap(),
            update_logger: |_| Ok(()),
        };
        for (event, error, expected) in [
            (
                crate::machine::MachineEvent::GuestReset,
                StartError::Resume(ResumeError(crate::machine::VcpuError::Exited(0))),
                VmmExitStatus::Ok,
            ),
            (
                crate::machine::MachineEvent::GuestReset,
                StartError::Resume(ResumeError(crate::machine::VcpuError::TimedOut(0))),
                VmmExitStatus::Error("resume the vCPUs: vCPU 0 did not answer in time".into()),
            ),
            (
                crate::machine::MachineEvent::GuestReset,
                StartError::Teardown(TeardownError::VmHeld),
                VmmExitStatus::Error("tear down the machine after a failed Start: the VM is still held after teardown".into()),
            ),
            (
                crate::machine::MachineEvent::VcpuFailed {
                    index: 0,
                    error: "injected fault".into(),
                },
                StartError::Resume(ResumeError(crate::machine::VcpuError::Exited(0))),
                VmmExitStatus::Error("vCPU 0 failed: injected fault".into()),
            ),
        ] {
            let (sender, machine) = tokio::sync::mpsc::unbounded_channel();
            let (_, devices) = crate::devices::runtime::DeviceRuntime::new();
            let events = MachineEvents { machine, devices };
            let (request, answer) = api::lifecycle::Start.into_vmm_request();
            let api::VmmRequest::Start(_, reply) = request else {
                unreachable!()
            };
            let operation = Box::pin(async move {
                sender.send(event).unwrap();
                Err(error)
            });
            let state = State::Busy(Busy::Starting(controller::Starting::new(
                MachineSpec::default(),
                reply,
                events,
                operation,
            )));
            let (_client, server) = crate::vmm_server::pair();
            let status = tokio::time::timeout(Duration::from_secs(3), serve(server, &ctx, state))
                .await
                .expect("the final-poll event was dropped with failed Start");
            assert_eq!(status, expected);
            assert!(answer.try_recv().unwrap().is_err());
        }
    }

    #[tokio::test(flavor = "local")]
    async fn guest_reset_during_an_in_flight_pause_on_kvm() {
        if !Path::new("/dev/kvm").exists() {
            return;
        }
        let kvm = KvmContext::from_dev_path("/dev/kvm").unwrap();
        // Wait for the host's flag, then reset via i8042. No timing sleeps.
        let mut code = vec![
            0x83, 0x3d, 0x00, 0x80, 0x00, 0x00, 0x00, // cmp dword [0x8000], 0
            0x74, 0xf7, // je back
        ];
        code.extend(elf::serial_reset_payload(false));
        let kernel = elf::TempFile::new(&elf::image(&code, true));
        let spec = [
            SetKernel(KernelConfig {
                path: kernel.path().into(),
                cmdline: None,
                initrd: None,
            })
            .into_vmm_command(),
            SetConsole(ConsoleConfig::None).into_vmm_command(),
            SetCpus(VcpuCount(2)).into_vmm_command(),
        ]
        .into_iter()
        .try_fold(MachineSpec::default(), |spec, op| spec.apply(op))
        .unwrap();
        let (plan, policy) = MachinePlan::new(&spec, kvm.capabilities()).unwrap();
        let (paused, events) = build(plan, policy, &kvm).unwrap();
        let machine = paused.resume().await.map_err(|(_, e)| e).unwrap();
        let memory = machine.memory().clone();
        let (request, answer) = Pause.into_vmm_request();
        let api::VmmRequest::Pause(_, reply) = request else {
            unreachable!()
        };
        let (entered, started) = oneshot::channel();
        let (release, wait) = oneshot::channel();
        // A controlled prelude models an in-flight bounded wait, then executes
        // the real transition. The operation owns the real KVM machine all
        // along; its loop must still receive the guest's *real* reset event.
        let pausing = Transition::new(Live { machine, events }, reply, |machine| {
            Box::pin(async move {
                entered.send(()).unwrap();
                tokio::time::timeout(Duration::from_secs(5), wait)
                    .await
                    .unwrap()
                    .unwrap();
                machine.pause().await
            })
        });
        let state = State::Busy(Busy::Pausing(pausing));
        let (client, server) = crate::vmm_server::pair();
        let ctx = ProcessCtx {
            id: "reset-during-pause".into(),
            version: "test".into(),
            kvm_ctx: kvm,
            update_logger: |_| Ok(()),
        };
        let trigger = async {
            started.await.unwrap();
            memory.write_obj(1u32, GuestAddress(0x8000)).unwrap();
            // The loop closes admission when it observes the reset, BEFORE
            // the gated operation completes. Queries must not hide the event.
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let (request, _) = api::instance_info::GetInstanceInfo.into_vmm_request();
                    if client.send(request).is_err() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("Busy failed to observe the guest reset");
            assert_eq!(answer.try_recv(), Err(::oneshot::TryRecvError::Empty));
            drop(memory);
            release.send(()).unwrap();
        };
        let (status, ()) = tokio::join!(serve(server, &ctx, state), trigger);
        assert_eq!(status, VmmExitStatus::Ok);
        assert_eq!(answer.try_recv(), Ok(Ok(())));
    }
}
