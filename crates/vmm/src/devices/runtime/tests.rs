use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::atomic::AtomicU32;
use std::{io, thread};

use tokio::select;

use super::*;
use crate::devices::async_fd::AsyncEventFd;
use crate::devices::control::Flow;

const TIMEOUT: Duration = Duration::from_secs(5);
const PLACEMENTS: [Placement; 2] = [Placement::VmmMain, Placement::Io];

/// Lets tasks on this thread run without waiting for anything specific.
async fn settle() {
    for _ in 0..20 {
        time::sleep(Duration::from_millis(1)).await;
    }
}

/// Waits for a stop and returns.
async fn idle<Q: Clone + Default>(mut control: Control<Q>) -> io::Result<()> {
    loop {
        let command = control.recv().await;
        if control.handle(command, Q::default).await == Flow::Stop {
            return Ok(());
        }
    }
}

/// Counts its own steps, reports each one, and parks at the count it reached.
async fn ticker(
    mut control: Control<u32>,
    steps: impl Fn(u32),
    ticks: mpsc::UnboundedSender<()>,
) -> io::Result<()> {
    let _not_send = Rc::new(());
    let mut count = 0;
    loop {
        select! {
            biased;
            command = control.recv() => {
                if control.handle(command, || count).await == Flow::Stop {
                    return Ok(());
                }
            }
            () = task::yield_now() => {
                count += 1;
                steps(count);
                let _ = ticks.send(());
            }
        }
    }
}

async fn panics_after_a_yield(_: Control<()>) -> io::Result<()> {
    task::yield_now().await;
    panic!("device future panic")
}

async fn panics_at_once(_: Control<()>) -> io::Result<()> {
    panic!("device panic")
}

async fn next_exit(exits: &mut DeviceExits) -> DeviceExit {
    time::timeout(TIMEOUT, exits.next()).await.unwrap().unwrap()
}

/// Shuts down, then checks that no exit beyond those already taken was
/// reported.
async fn shutdown_quietly(runtime: DeviceRuntime, mut exits: DeviceExits) {
    time::timeout(TIMEOUT, runtime.shutdown(TIMEOUT))
        .await
        .unwrap()
        .unwrap();
    let rest = time::timeout(TIMEOUT, exits.next()).await.unwrap();
    assert!(rest.is_none(), "unexpected exit {rest:?}");
}

#[tokio::test(flavor = "local")]
async fn local_spawn_accepts_non_send_future_and_preserves_errors() {
    let (mut runtime, mut exits) = DeviceRuntime::new();
    let state = Rc::new(42);
    let handle = runtime
        .spawn("local", Start::Running, move |_: Control<()>| async move {
            task::yield_now().await;
            assert_eq!(*state, 42);
            Err(io::Error::other("device failure"))
        })
        .unwrap();
    assert_eq!(handle.name(), "local");
    let exit = next_exit(&mut exits).await;
    assert_eq!(&*exit.name, "local");
    let Err(DeviceFailure::Failed(error)) = exit.result else {
        panic!("unexpected result {:?}", exit.result);
    };
    assert_eq!(error.to_string(), "device failure");
    assert!(handle.has_exited());
    assert_eq!(runtime.running(), 0);
    assert!(runtime.io_thread.is_none());
    shutdown_quietly(runtime, exits).await;
}

#[tokio::test(flavor = "local")]
async fn io_factory_constructs_non_send_future_on_target_runtime() {
    let (mut runtime, mut exits) = DeviceRuntime::new();
    let main_thread = thread::current().id();
    let (ready_tx, ready_rx) = oneshot::channel();
    let _handle = runtime
        .spawn_on("io", Placement::Io, Start::Running, move |_: Control<()>| {
            let state = Rc::new(thread::current().id());
            let event = AsyncEventFd::new().unwrap();
            ready_tx.send((*state, event.notifier().unwrap())).unwrap();
            async move {
                assert_eq!(event.read().await?, 1);
                assert_eq!(*state, thread::current().id());
                Ok::<_, io::Error>(())
            }
        })
        .unwrap();
    let (io_thread, notifier) = time::timeout(TIMEOUT, ready_rx).await.unwrap().unwrap();
    assert_ne!(main_thread, io_thread);
    notifier.write(1).unwrap();
    let exit = next_exit(&mut exits).await;
    assert_eq!(&*exit.name, "io");
    exit.result.unwrap();
    assert_eq!(runtime.running(), 0);
    shutdown_quietly(runtime, exits).await;
}

#[tokio::test(flavor = "local")]
async fn shutdown_stops_local_and_io_devices_and_joins_the_io_thread() {
    let (mut runtime, exits) = DeviceRuntime::new();
    let main_thread = thread::current().id();
    let mut handles = Vec::new();
    let mut completions = Vec::new();
    for placement in PLACEMENTS {
        let (done_tx, done_rx) = oneshot::channel();
        let handle = runtime
            .spawn_on(
                format!("{placement:?}"),
                placement,
                Start::Running,
                move |control: Control<()>| {
                    let state = Rc::new(thread::current().id());
                    async move {
                        idle(control).await?;
                        done_tx.send(*state).unwrap();
                        Ok::<_, io::Error>(())
                    }
                },
            )
            .unwrap();
        handles.push(handle);
        completions.push((placement, done_rx));
    }
    assert_eq!(runtime.running(), 2);
    assert!(runtime.io_thread.is_some());
    shutdown_quietly(runtime, exits).await;
    for (placement, completion) in completions {
        let device_thread = completion.await.unwrap();
        match placement {
            Placement::VmmMain => assert_eq!(device_thread, main_thread),
            Placement::Io => assert_ne!(device_thread, main_thread),
        }
    }
    for handle in &handles {
        assert!(handle.has_exited());
        time::timeout(TIMEOUT, handle.finished()).await.unwrap();
        handle.stop();
        assert_eq!(handle.quiesce(TIMEOUT).await, Response::Exited);
        assert_eq!(handle.resume(TIMEOUT).await, Response::Exited);
    }
}

#[tokio::test(flavor = "local")]
async fn holders_stop_their_own_tasks_by_request_or_by_dropping_the_handle() {
    let (mut runtime, exits) = DeviceRuntime::new();
    for placement in PLACEMENTS {
        let requested = runtime
            .spawn_on("requested", placement, Start::Running, idle::<()>)
            .unwrap();
        let dropped = runtime
            .spawn_on("dropped", placement, Start::Running, idle::<()>)
            .unwrap();
        let watcher = dropped.finished.clone();
        requested.stop();
        time::timeout(TIMEOUT, requested.finished()).await.unwrap();
        drop(dropped);
        time::timeout(TIMEOUT, watcher.clone().wait())
            .await
            .unwrap();
        assert!(watcher.is_done());
    }
    assert_eq!(runtime.running(), 0);
    // Neither stop is an unexpected exit.
    shutdown_quietly(runtime, exits).await;
}

#[tokio::test(flavor = "local")]
async fn future_and_factory_panics_report_exactly_one_exit() {
    let (mut runtime, mut exits) = DeviceRuntime::new();
    let mut handles = Vec::new();
    for placement in PLACEMENTS {
        handles.push(
            runtime
                .spawn_on(
                    format!("future-{placement:?}"),
                    placement,
                    Start::Running,
                    panics_after_a_yield,
                )
                .unwrap(),
        );
        handles.push(
            runtime
                .spawn_on(
                    format!("factory-{placement:?}"),
                    placement,
                    Start::Running,
                    |_: Control<()>| -> std::future::Ready<io::Result<()>> {
                        panic!("device factory panic")
                    },
                )
                .unwrap(),
        );
    }
    let mut names = HashSet::new();
    for _ in 0..4 {
        let exit = next_exit(&mut exits).await;
        assert!(names.insert(exit.name.clone()), "{} twice", exit.name);
        assert!(
            matches!(exit.result, Err(DeviceFailure::Panicked(ref error)) if error.is_panic()),
            "{exit:?}"
        );
    }
    assert_eq!(names.len(), 4);
    for handle in &handles {
        time::timeout(TIMEOUT, handle.finished()).await.unwrap();
    }
    assert_eq!(runtime.running(), 0);
    shutdown_quietly(runtime, exits).await;
}

#[tokio::test(flavor = "local")]
async fn shutdown_does_not_hang_when_devices_panic() {
    let (mut runtime, exits) = DeviceRuntime::new();
    let mut handles = Vec::new();
    for placement in PLACEMENTS {
        handles.push(
            runtime
                .spawn_on(
                    format!("panic-{placement:?}"),
                    placement,
                    Start::Running,
                    panics_at_once,
                )
                .unwrap(),
        );
    }
    time::timeout(TIMEOUT, runtime.shutdown(TIMEOUT))
        .await
        .unwrap()
        .unwrap();
    for handle in &handles {
        assert!(handle.has_exited());
    }
    drop(exits);
}

#[test]
fn spawning_without_a_local_runtime_is_an_error_and_tracks_nothing() {
    let (mut runtime, _exits) = DeviceRuntime::new();
    let spawned = runtime.spawn("no-runtime", Start::Running, idle::<()>);
    assert!(matches!(spawned, Err(SpawnError::NoLocalRuntime)));
    let spawned = runtime.spawn_on("no-runtime", Placement::VmmMain, Start::Running, idle::<()>);
    assert!(matches!(spawned, Err(SpawnError::NoLocalRuntime)));
    assert_eq!(runtime.running(), 0);
    assert!(runtime.tasks.is_empty());
    assert!(runtime.io_thread.is_none());

    // A runtime that is not local cannot take `!Send` tasks either.
    let current = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let _guard = current.enter();
    let spawned = runtime.spawn("not-local", Start::Running, idle::<()>);
    assert!(matches!(spawned, Err(SpawnError::NoLocalRuntime)));
    assert!(runtime.tasks.is_empty());
}

#[tokio::test(flavor = "local")]
async fn quiesce_parks_a_vmm_main_non_send_future_and_resume_continues_it() {
    let (mut runtime, exits) = DeviceRuntime::new();
    let counter = Rc::new(Cell::new(0));
    let steps = Rc::clone(&counter);
    let (tick_tx, mut ticks) = mpsc::unbounded_channel();
    let handle = runtime
        .spawn("ticker", Start::Running, move |control| {
            ticker(control, move |count| steps.set(count), tick_tx)
        })
        .unwrap();
    time::timeout(TIMEOUT, ticks.recv()).await.unwrap().unwrap();

    let Response::Replied(parked_at) = handle.quiesce(TIMEOUT).await else {
        panic!("ticker did not quiesce");
    };
    assert_eq!(parked_at, counter.get());
    while ticks.try_recv().is_ok() {}
    settle().await;
    assert_eq!(counter.get(), parked_at, "advanced while quiesced");

    assert_eq!(handle.resume(TIMEOUT).await, Response::Replied(()));
    time::timeout(TIMEOUT, ticks.recv()).await.unwrap().unwrap();
    // The same future carries on from where it parked.
    let Response::Replied(later) = handle.quiesce(TIMEOUT).await else {
        panic!("ticker did not quiesce again");
    };
    assert!(later > parked_at);
    shutdown_quietly(runtime, exits).await;
}

#[tokio::test(flavor = "local")]
async fn quiesce_parks_an_io_non_send_future_and_resume_continues_it() {
    let (mut runtime, exits) = DeviceRuntime::new();
    let counter = Arc::new(AtomicU32::new(0));
    let steps = Arc::clone(&counter);
    let (tick_tx, mut ticks) = mpsc::unbounded_channel();
    let handle = runtime
        .spawn_on("io-ticker", Placement::Io, Start::Running, move |control| {
            ticker(
                control,
                move |count| steps.store(count, Ordering::SeqCst),
                tick_tx,
            )
        })
        .unwrap();
    time::timeout(TIMEOUT, ticks.recv()).await.unwrap().unwrap();

    let Response::Replied(parked_at) = handle.quiesce(TIMEOUT).await else {
        panic!("ticker did not quiesce");
    };
    assert_eq!(parked_at, counter.load(Ordering::SeqCst));
    while ticks.try_recv().is_ok() {}
    settle().await;
    assert_eq!(counter.load(Ordering::SeqCst), parked_at);

    assert_eq!(handle.resume(TIMEOUT).await, Response::Replied(()));
    time::timeout(TIMEOUT, ticks.recv()).await.unwrap().unwrap();
    assert!(counter.load(Ordering::SeqCst) > parked_at);
    shutdown_quietly(runtime, exits).await;
}

#[tokio::test(flavor = "local")]
async fn a_host_only_device_may_answer_quiesce_and_keep_running() {
    let (mut runtime, exits) = DeviceRuntime::new();
    let (tick_tx, mut ticks) = mpsc::unbounded_channel();
    let handle = runtime
        .spawn(
            "host-only",
            Start::Running,
            move |mut control: Control<()>| async move {
                loop {
                    select! {
                        biased;
                        command = control.recv() => match command {
                            Command::Quiesce(ack) | Command::Resume(ack) => ack.send(()),
                            Command::Stop => return Ok::<_, io::Error>(()),
                        },
                        () = task::yield_now() => { let _ = tick_tx.send(()); }
                    }
                }
            },
        )
        .unwrap();
    time::timeout(TIMEOUT, ticks.recv()).await.unwrap().unwrap();
    assert_eq!(handle.quiesce(TIMEOUT).await, Response::Replied(()));
    while ticks.try_recv().is_ok() {}
    time::timeout(TIMEOUT, ticks.recv()).await.unwrap().unwrap();
    shutdown_quietly(runtime, exits).await;
}

#[tokio::test(flavor = "local")]
async fn shutdown_from_running_and_quiesced_states_with_the_io_thread() {
    for quiesce in [false, true] {
        let (mut runtime, exits) = DeviceRuntime::new();
        let mut handles = Vec::new();
        for placement in PLACEMENTS {
            let handle = runtime
                .spawn_on(
                    format!("{placement:?}"),
                    placement,
                    Start::Running,
                    idle::<u8>,
                )
                .unwrap();
            if quiesce {
                assert_eq!(handle.quiesce(TIMEOUT).await, Response::Replied(0));
            }
            handles.push(handle);
        }
        assert_eq!(runtime.running(), 2);
        shutdown_quietly(runtime, exits).await;
        assert!(handles.iter().all(DeviceHandle::has_exited));
    }
}

#[tokio::test(flavor = "local")]
async fn quiesce_and_resume_are_idempotent() {
    let (mut runtime, exits) = DeviceRuntime::new();
    let (tick_tx, mut ticks) = mpsc::unbounded_channel();
    let handle = runtime
        .spawn("ticker", Start::Running, move |control| {
            ticker(control, |_| {}, tick_tx)
        })
        .unwrap();
    time::timeout(TIMEOUT, ticks.recv()).await.unwrap().unwrap();

    let first = handle.quiesce(TIMEOUT).await;
    let second = handle.quiesce(TIMEOUT).await;
    assert!(matches!(first, Response::Replied(_)));
    assert_eq!(first, second);
    while ticks.try_recv().is_ok() {}

    assert_eq!(handle.resume(TIMEOUT).await, Response::Replied(()));
    assert_eq!(handle.resume(TIMEOUT).await, Response::Replied(()));
    time::timeout(TIMEOUT, ticks.recv()).await.unwrap().unwrap();
    shutdown_quietly(runtime, exits).await;
}

#[tokio::test(flavor = "local")]
async fn a_task_spawned_parked_starts_at_its_first_resume() {
    let (mut runtime, exits) = DeviceRuntime::new();
    for placement in PLACEMENTS {
        let (tick_tx, mut ticks) = mpsc::unbounded_channel();
        let handle = runtime
            .spawn_on("parked", placement, Start::Parked, move |control| {
                ticker(control, |_| {}, tick_tx)
            })
            .unwrap();
        settle().await;
        assert!(ticks.try_recv().is_err(), "a parked task ran");
        // Quiescing before the first resume reports the initial state.
        assert_eq!(handle.quiesce(TIMEOUT).await, Response::Replied(0));
        settle().await;
        assert!(ticks.try_recv().is_err(), "a parked task ran");

        assert_eq!(handle.resume(TIMEOUT).await, Response::Replied(()));
        time::timeout(TIMEOUT, ticks.recv()).await.unwrap().unwrap();
    }
    shutdown_quietly(runtime, exits).await;
}

#[tokio::test(flavor = "local")]
async fn a_task_spawned_parked_can_be_stopped_before_it_starts() {
    let (mut runtime, exits) = DeviceRuntime::new();
    let started = Rc::new(Cell::new(false));
    let flag = Rc::clone(&started);
    let _handle = runtime
        .spawn("parked", Start::Parked, move |control: Control<()>| {
            ticker_with_flag(control, flag)
        })
        .unwrap();
    shutdown_quietly(runtime, exits).await;
    assert!(!started.get());
}

async fn ticker_with_flag(mut control: Control<()>, started: Rc<Cell<bool>>) -> io::Result<()> {
    loop {
        select! {
            biased;
            command = control.recv() => {
                if control.handle(command, || ()).await == Flow::Stop {
                    return Ok(());
                }
            }
            () = task::yield_now() => started.set(true),
        }
    }
}

#[tokio::test(flavor = "local")]
async fn quiescing_an_exited_task_reports_that_it_exited() {
    let (mut runtime, mut exits) = DeviceRuntime::new();
    for placement in PLACEMENTS {
        let handle = runtime
            .spawn_on(
                "short-lived",
                placement,
                Start::Running,
                |_: Control<u8>| async { Ok::<_, io::Error>(()) },
            )
            .unwrap();
        let exit = next_exit(&mut exits).await;
        assert!(exit.result.is_ok());
        time::timeout(TIMEOUT, handle.finished()).await.unwrap();
        assert_eq!(handle.quiesce(TIMEOUT).await, Response::Exited);
        assert_eq!(handle.resume(TIMEOUT).await, Response::Exited);
    }
    // A request queued just before a task ends is answered as an exit too.
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let handle = runtime
        .spawn("ending", Start::Running, move |_: Control<u8>| async move {
            let _ = release_rx.await;
            Ok::<_, io::Error>(())
        })
        .unwrap();
    let quiescing = handle.quiesce(TIMEOUT);
    let release = async {
        settle().await;
        release_tx.send(()).unwrap();
    };
    let (response, ()) = tokio::join!(quiescing, release);
    assert_eq!(response, Response::Exited);
    next_exit(&mut exits).await.result.unwrap();
    shutdown_quietly(runtime, exits).await;
}

#[tokio::test(flavor = "local")]
async fn an_unresponsive_task_is_reported_not_awaited() {
    let (mut runtime, exits) = DeviceRuntime::new();
    let (release_tx, release_rx) = oneshot::channel::<()>();
    // Holds its control but waits for something else first.
    let handle = runtime
        .spawn(
            "stuck",
            Start::Running,
            move |control: Control<u8>| async move {
                let _ = release_rx.await;
                idle(control).await
            },
        )
        .unwrap();
    let short = Duration::from_millis(20);
    assert_eq!(handle.quiesce(short).await, Response::TimedOut);
    assert_eq!(handle.resume(short).await, Response::TimedOut);
    let stuck = runtime.shutdown(short).await;
    let Err(ShutdownError::Unresponsive(names)) = stuck else {
        panic!("unexpected shutdown result {stuck:?}");
    };
    assert_eq!(names, [Arc::from("stuck")]);
    // Once it gets going it answers what is queued, including the stop.
    release_tx.send(()).unwrap();
    time::timeout(TIMEOUT, handle.finished()).await.unwrap();
    drop(exits);
}

#[tokio::test(flavor = "local")]
async fn shutdown_wait_retains_device_resources_until_released_on_each_placement() {
    for placement in PLACEMENTS {
        let (mut runtime, exits) = DeviceRuntime::new();
        let resource = Arc::new(());
        let held = Arc::downgrade(&resource);
        let (stopped_tx, stopped_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let handle = runtime
            .spawn_on(
                "pending-cleanup",
                placement,
                Start::Running,
                move |control: Control<()>| async move {
                    let _resource = resource;
                    idle(control).await?;
                    stopped_tx.send(()).unwrap();
                    release_rx.await.unwrap();
                    Ok::<_, io::Error>(())
                },
            )
            .unwrap();

        let result = runtime.shutdown_wait(Duration::ZERO).await;
        assert!(
            matches!(result, Err(ShutdownError::Unresponsive(ref names))
                if names == &[Arc::<str>::from("pending-cleanup")]),
            "{result:?}"
        );
        assert!(held.upgrade().is_some());
        assert!(!handle.has_exited());
        time::timeout(TIMEOUT, stopped_rx).await.unwrap().unwrap();
        let result = runtime.shutdown_wait(Duration::ZERO).await;
        assert!(matches!(result, Err(ShutdownError::Unresponsive(_))));
        assert!(held.upgrade().is_some());
        assert!(!handle.has_exited());
        assert_eq!(runtime.running(), 1);
        assert_eq!(runtime.tasks.len(), 1);
        assert_eq!(runtime.io_thread.is_some(), placement == Placement::Io);

        release_tx.send(()).unwrap();
        runtime.shutdown_wait(TIMEOUT).await.unwrap();
        assert!(held.upgrade().is_none());
        assert!(handle.has_exited());
        assert!(runtime.tasks.is_empty());
        assert!(runtime.io_thread.is_none());
        runtime.shutdown_wait(Duration::ZERO).await.unwrap();
        shutdown_quietly(runtime, exits).await;
    }
}

#[tokio::test(flavor = "local")]
async fn shutdown_wait_retains_a_pending_io_join_and_consumes_completed_errors() {
    for panics in [false, true] {
        let (mut runtime, exits) = DeviceRuntime::new();
        let handle = runtime
            .spawn_on("io", Placement::Io, Start::Running, idle::<()>)
            .unwrap();
        handle.stop();
        time::timeout(TIMEOUT, handle.finished()).await.unwrap();

        let resource = Arc::new(());
        let held = Arc::downgrade(&resource);
        let (ready_tx, ready_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        runtime
            .io_thread
            .as_ref()
            .unwrap()
            .run(Box::new(move || {
                let _resource = resource;
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                if panics {
                    panic!("I/O thread cleanup panic");
                }
            }))
            .unwrap();
        time::timeout(TIMEOUT, ready_rx).await.unwrap().unwrap();

        for _ in 0..2 {
            let result = runtime.shutdown_wait(Duration::ZERO).await;
            assert!(
                matches!(result, Err(ShutdownError::IoJoinTimeout(_))),
                "{result:?}"
            );
            assert!(held.upgrade().is_some());
            assert_eq!(runtime.tasks.len(), 1);
            assert!(handle.has_exited());
            assert!(matches!(
                runtime.io_thread.as_ref().unwrap().run(Box::new(|| {})),
                Err(IoThreadError::Closed)
            ));
        }

        release_tx.send(()).unwrap();
        let result = runtime.shutdown_wait(TIMEOUT).await;
        if panics {
            assert!(matches!(
                result,
                Err(ShutdownError::IoThread(IoThreadError::Panicked))
            ));
        } else {
            result.unwrap();
        }
        assert!(held.upgrade().is_none());
        assert!(runtime.tasks.is_empty());
        assert!(runtime.io_thread.is_none());
        runtime.shutdown_wait(Duration::ZERO).await.unwrap();
        shutdown_quietly(runtime, exits).await;
    }
}

#[tokio::test(flavor = "local")]
async fn the_exit_stream_can_be_owned_elsewhere() {
    let (mut runtime, mut exits) = DeviceRuntime::new();
    let (seen_tx, seen_rx) = oneshot::channel();
    let consumer = task::spawn_local(async move {
        let mut seen = Vec::new();
        while let Some(exit) = exits.next().await {
            seen.push(exit.name);
        }
        seen_tx.send(seen).unwrap();
    });
    let mut handles = Vec::new();
    for placement in PLACEMENTS {
        handles.push(
            runtime
                .spawn_on(
                    format!("failing-{placement:?}"),
                    placement,
                    Start::Running,
                    |_: Control<()>| async { Err(io::Error::other("failed")) },
                )
                .unwrap(),
        );
        handles.push(
            runtime
                .spawn_on("stopped", placement, Start::Running, idle::<()>)
                .unwrap(),
        );
    }
    // Failing after shutdown has asked them to stop would be expected.
    for failing in handles.iter().step_by(2) {
        time::timeout(TIMEOUT, failing.finished()).await.unwrap();
    }
    time::timeout(TIMEOUT, runtime.shutdown(TIMEOUT))
        .await
        .unwrap()
        .unwrap();
    let mut seen = time::timeout(TIMEOUT, seen_rx).await.unwrap().unwrap();
    seen.sort();
    assert_eq!(
        seen,
        [Arc::from("failing-Io"), Arc::from("failing-VmmMain")]
    );
    consumer.await.unwrap();
}
