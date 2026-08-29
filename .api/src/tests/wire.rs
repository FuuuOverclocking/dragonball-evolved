//! The `wire` side of the registry: commands become runnable requests, typed
//! replies become JSON, and what that JSON accepts and refuses is checked too.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use serde_json::{Value, json};

use super::*;
use crate::wire::{ReplyError, ReplyWaiter};

#[test]
fn preparing_a_command_attaches_the_channel() {
    let (request, waiter) = Command::Add(Add { lhs: 2, rhs: 3 }, ()).prepare();
    assert_eq!(request.name(), "Add");

    serve(request);

    // A business `Result` travels as data, like any other reply.
    assert_eq!(poll_once(waiter).expect("answered"), json!({ "Ok": 5 }));
}

#[test]
fn waiters_for_different_operations_wait_alike() {
    let (add, add_waiter) = Command::Add(Add { lhs: 1, rhs: 1 }, ()).prepare();
    let (echo, echo_waiter) = Command::Echo(Echo { text: "hi".into() }, ()).prepare();

    // One collection holding two reply types is what the erased boundary buys.
    let waiters: Vec<ReplyWaiter> = vec![add_waiter, echo_waiter];
    serve(add);
    serve(echo);

    let answers: Vec<_> = waiters
        .into_iter()
        .map(|waiter| poll_once(waiter).expect("answered"))
        .collect();
    assert_eq!(answers, vec![json!({ "Ok": 2 }), json!("hi")]);
}

#[test]
fn a_refused_operation_arrives_as_data() {
    let (request, waiter) = Command::Add(Add { lhs: 1, rhs: -1 }, ()).prepare();
    serve(request);

    assert_eq!(
        poll_once(waiter).expect("answered"),
        json!({ "Err": "Forbidden" })
    );
}

#[test]
fn an_answer_json_cannot_hold_is_reported() {
    let (request, waiter) = Command::Opaque(Opaque {}, ()).prepare();
    serve(request);

    assert!(matches!(
        poll_once(waiter),
        Err(ReplyError::Serialization(_))
    ));
}

#[test]
fn a_vmm_that_never_answers_disconnects_the_waiter() {
    let (request, waiter) = Command::Echo(Echo { text: "hi".into() }, ()).prepare();

    drop(request);

    assert!(matches!(poll_once(waiter), Err(ReplyError::Disconnected)));
}

#[test]
fn dropping_a_waiter_leaves_the_operation_running() {
    let (request, waiter) = Command::Echo(Echo { text: "hi".into() }, ()).prepare();
    drop(waiter);

    // The vmm answers into a channel nobody holds, which is not a failure.
    assert_eq!(serve(request), "Echo");
}

#[test]
fn an_answer_from_another_thread_wakes_the_registered_waker() {
    let (request, mut waiter) = Command::Echo(Echo { text: "hi".into() }, ()).prepare();
    let woken = Arc::new(AtomicBool::new(false));
    let waker = Waker::from(Arc::new(Flag(Arc::clone(&woken))));

    // Before the reply there is nothing to hand over...
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(waiter.as_mut().poll(&mut cx), Poll::Pending));

    // ...and the answer must wake the waiter rather than be missed.
    let served = std::thread::spawn(move || serve(request))
        .join()
        .expect("the vmm side finished");
    assert_eq!(served, "Echo");
    assert!(woken.load(Ordering::Acquire), "the answer woke the waiter");

    match waiter.as_mut().poll(&mut cx) {
        Poll::Ready(value) => assert_eq!(value.expect("answered"), json!("hi")),
        Poll::Pending => panic!("a woken waiter answers"),
    }
}

#[test]
fn a_dropped_request_wakes_the_waiter_with_disconnected() {
    let (request, mut waiter) = Command::Echo(Echo { text: "hi".into() }, ()).prepare();
    let woken = Arc::new(AtomicBool::new(false));
    let waker = Waker::from(Arc::new(Flag(Arc::clone(&woken))));

    let mut cx = Context::from_waker(&waker);
    assert!(matches!(waiter.as_mut().poll(&mut cx), Poll::Pending));

    std::thread::spawn(move || drop(request))
        .join()
        .expect("the request was dropped");
    assert!(
        woken.load(Ordering::Acquire),
        "disconnecting woke the waiter"
    );

    match waiter.as_mut().poll(&mut cx) {
        Poll::Ready(value) => assert!(matches!(value, Err(ReplyError::Disconnected))),
        Poll::Pending => panic!("a woken waiter answers"),
    }
}

#[test]
fn dropping_a_registered_waiter_still_lets_the_vmm_finish() {
    let (request, mut waiter) = Command::Echo(Echo { text: "hi".into() }, ()).prepare();
    let waker = Waker::from(Arc::new(Flag(Arc::new(AtomicBool::new(false)))));

    {
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(waiter.as_mut().poll(&mut cx), Poll::Pending));
    }

    // The waker registration went away with the waiter; the vmm cannot notice.
    drop(waiter);
    drop(waker);

    assert_eq!(serve(request), "Echo");
}

#[test]
fn a_command_is_the_serializable_shape_of_a_request() {
    let value = serde_json::to_value(Command::Add(Add { lhs: 2, rhs: 3 }, ())).expect("serialize");
    assert_eq!(value, json!({ "Add": { "lhs": 2, "rhs": 3 } }));

    match serde_json::from_value::<Command>(value).expect("deserialize") {
        Command::Add(params, ()) => assert_eq!(params, Add { lhs: 2, rhs: 3 }),
        other => panic!("unexpected operation: {other:?}"),
    }
}

#[test]
fn the_production_commands_serialize_too() {
    let command = crate::Command::Shutdown(crate::Shutdown {}, ());
    assert_eq!(
        serde_json::to_value(&command).expect("serialize"),
        json!({ "Shutdown": {} })
    );

    let parsed: crate::Command =
        serde_json::from_value(json!({ "GetInstanceInfo": {} })).expect("deserialize");
    assert_eq!(parsed.name(), "GetInstanceInfo");
}

#[test]
fn a_command_with_parameters_round_trips_through_raw_json() {
    let json = r#"{"Add":{"lhs":2,"rhs":3}}"#;
    match serde_json::from_str::<Command>(json).expect("deserialize") {
        Command::Add(params, ()) => assert_eq!(params, Add { lhs: 2, rhs: 3 }),
        other => panic!("unexpected operation: {other:?}"),
    }

    assert_eq!(
        serde_json::to_string(&Command::Add(Add { lhs: 2, rhs: 3 }, ())).expect("serialize"),
        json
    );
}

#[test]
fn the_production_replies_round_trip() {
    let status = crate::VmmStatus { devices_running: 3 };
    assert_eq!(
        serde_json::to_string(&status).expect("serialize"),
        r#"{"devices_running":3}"#
    );
    assert_eq!(
        serde_json::from_str::<crate::VmmStatus>(r#"{"devices_running":3}"#).expect("deserialize"),
        status
    );

    let info = crate::InstanceInfo { id: "vm-1".into() };
    assert_eq!(
        serde_json::to_string(&info).expect("serialize"),
        r#"{"id":"vm-1"}"#
    );
    assert_eq!(
        serde_json::from_str::<crate::InstanceInfo>(r#"{"id":"vm-1"}"#).expect("deserialize"),
        info
    );
}

#[test]
fn a_business_result_round_trips_as_data() {
    let answers: [Result<i64, AddError>; 2] = [Ok(5), Err(AddError::Forbidden)];

    for answer in &answers {
        let json = serde_json::to_string(answer).expect("serialize");
        assert_eq!(
            serde_json::from_str::<Result<i64, AddError>>(&json).expect("deserialize"),
            *answer
        );
    }

    // The shape a caller sees, carrying the error the operation declared.
    assert_eq!(
        serde_json::to_string(&answers[1]).expect("serialize"),
        r#"{"Err":"Forbidden"}"#
    );
}

#[test]
fn a_production_command_prepares_into_a_waiter() {
    let (request, waiter) = crate::Command::Status(crate::Status {}, ()).prepare();

    // Standing in for the vmm: the reply type is the one the operation declared.
    match request {
        crate::VmmRequest::Status(_, reply) => reply.send(crate::VmmStatus { devices_running: 1 }),
        other => panic!("unexpected request: {other:?}"),
    }

    assert_eq!(
        poll_once(waiter).expect("answered"),
        json!({ "devices_running": 1 })
    );
}

#[test]
fn a_parameter_type_keeps_its_own_serde_shape() {
    // The field is `name` in Rust and `who` on the wire: the registry carried
    // the parameter type through instead of rewriting its attributes.
    assert_eq!(
        serde_json::to_value(Command::Greet(Greet { name: "vm".into() }, ())).expect("serialize"),
        json!({ "Greet": { "who": "vm" } })
    );

    // Its default applies on the wire, too.
    match serde_json::from_value::<Command>(json!({ "Greet": {} })).expect("deserialize") {
        Command::Greet(params, ()) => assert_eq!(params.name, "world"),
        other => panic!("unexpected operation: {other:?}"),
    }

    // And the Rust name is unknown on the wire, as the rename dictates.
    let renamed = serde_json::from_str::<Command>(r#"{"Greet": {"name": "vm"}}"#)
        .expect_err("the wire knows the field as `who`");
    assert!(
        renamed.to_string().contains("unknown field `name`"),
        "{renamed}"
    );
}

#[test]
fn a_command_names_exactly_one_operation() {
    assert!(serde_json::from_value::<crate::Command>(json!({ "Reboot": {} })).is_err());
    assert!(
        serde_json::from_value::<crate::Command>(json!({ "Shutdown": {}, "Status": {} })).is_err()
    );
    // Nothing named at all leaves nothing to run.
    assert!(serde_json::from_value::<crate::Command>(json!({})).is_err());
    assert!(serde_json::from_value::<crate::Command>(json!([])).is_err());
}

#[test]
fn a_command_refuses_parameters_its_operation_did_not_declare() {
    let extra = serde_json::from_str::<Command>(r#"{"Add": {"lhs": 1, "rhs": 2, "sum": 3}}"#)
        .expect_err("an undeclared parameter is a misspelling, not a request to ignore");
    assert!(extra.to_string().contains("unknown field `sum`"), "{extra}");

    // The same for an operation that takes none: it has no field to hide one in.
    let none = serde_json::from_str::<crate::Command>(r#"{"Shutdown": {"now": true}}"#)
        .expect_err("Shutdown declared no parameters");
    assert!(none.to_string().contains("unknown field `now`"), "{none}");
}

#[test]
fn a_command_refuses_incomplete_or_mistyped_parameters() {
    let missing =
        serde_json::from_str::<Command>(r#"{"Add": {"lhs": 1}}"#).expect_err("rhs is required");
    assert!(
        missing.to_string().contains("missing field `rhs`"),
        "{missing}"
    );

    let mistyped = serde_json::from_str::<Command>(r#"{"Add": {"lhs": "one", "rhs": 2}}"#)
        .expect_err("lhs is an i64");
    assert!(mistyped.to_string().contains("invalid type"), "{mistyped}");
}

#[test]
fn raw_json_cannot_repeat_an_operation_or_a_parameter() {
    // A `Value` would collapse the duplicate keys before the deserializer saw them,
    // so these go in as text.
    let twice = serde_json::from_str::<crate::Command>(r#"{"Shutdown": {}, "Shutdown": {}}"#)
        .expect_err("one request names one operation");
    assert!(
        twice.to_string().contains("exactly one operation"),
        "{twice}"
    );

    let repeated = serde_json::from_str::<Command>(r#"{"Echo": {"text": "a", "text": "b"}}"#)
        .expect_err("a parameter is named once");
    assert!(
        repeated.to_string().contains("duplicate field `text`"),
        "{repeated}"
    );
}

/// A waker that only records that it fired: these tests assert on wakeups, they
/// never park.
struct Flag(Arc<AtomicBool>);

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }
}

/// Poll a waiter whose request was answered first, so `Pending` is a missed
/// answer to fail on rather than something to wait out.
fn poll_once(mut waiter: ReplyWaiter) -> Result<Value, ReplyError> {
    let mut cx = Context::from_waker(Waker::noop());
    match waiter.as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("an answered request is ready on its first poll"),
    }
}
