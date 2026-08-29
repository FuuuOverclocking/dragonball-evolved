//! The generated shapes, driven by a registry of their own.
//!
//! The three production operations take no parameters and cannot fail, which would
//! hide most of what the macro has to get right. This registry has parameters, a
//! `Result` reply, a reply that JSON cannot hold, a parameter type with its own
//! Serde attributes and an operation with no doc comment at all. Its enum and its
//! command alias are named here rather than by the macro.

use serde::{Deserialize, Serialize};

use crate::reply_channel;

mod params;

use params::{Add, Echo, Greet, Noop, Opaque};

define_schema! {
    TestRequest {
        /// Add two numbers.
        /// Refuses the ones this test wants to see fail.
        Add -> Result<i64, AddError>;

        /// Hand the caller's own text back.
        Echo -> String;

        /// Answer with something JSON has no representation of.
        Opaque -> OpaqueValue;

        Noop -> ();

        /// Greet someone, through a parameter type with its own Serde shape.
        Greet -> String;
    }
}

/// This registry's channel-free request: the macro supplies the enum, the
/// invocation supplies every name around it.
type Command = TestRequest<crate::mode::Command>;

/// Why an addition was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AddError {
    Forbidden,
}

/// A reply that refuses to be serialized.
pub struct OpaqueValue;

impl Serialize for OpaqueValue {
    fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom("no JSON representation"))
    }
}

/// Stand in for the vmm: answer each request with what its own operation declared.
fn serve(request: TestRequest) -> &'static str {
    let name = request.name();
    match request {
        TestRequest::Add(params, reply) => reply.send(if params.rhs < 0 {
            Err(AddError::Forbidden)
        } else {
            Ok(params.lhs + params.rhs)
        }),
        TestRequest::Echo(params, reply) => reply.send(params.text),
        TestRequest::Opaque(_, reply) => reply.send(OpaqueValue),
        TestRequest::Noop(_, reply) => reply.send(()),
        TestRequest::Greet(params, reply) => reply.send(format!("hello {}", params.name)),
    }
    name
}

#[test]
fn a_command_holds_the_parameters_and_no_channel() {
    let command = Command::Add(Add { lhs: 2, rhs: 3 }, ());
    assert_eq!(command.name(), "Add");

    // The reply slot is empty: nothing to allocate, nothing to disconnect.
    match &command {
        Command::Add(params, ()) => assert_eq!((params.lhs, params.rhs), (2, 3)),
        other => panic!("unexpected operation: {other:?}"),
    }
}

#[test]
fn the_default_mode_is_the_runnable_one() {
    // Written without a mode, the enum is the runnable one...
    let runnable: TestRequest = TestRequest::Noop(Noop {}, crate::reply_channel().0);
    assert_eq!(runnable.name(), "Noop");

    // ...and the explicit command mode is a different type of the same enum.
    let data: TestRequest<crate::mode::Command> = TestRequest::Noop(Noop {}, ());
    assert_eq!(data.name(), "Noop");
}

#[test]
fn operations_describe_themselves_in_declaration_order() {
    let names: Vec<&str> = OPERATIONS.iter().map(|op| op.name).collect();
    assert_eq!(names, ["Add", "Echo", "Opaque", "Noop", "Greet"]);

    // Each `///` line arrives as rustdoc left it, leading space included, and an
    // operation without one gets an empty string rather than attribute source.
    let docs: Vec<&str> = OPERATIONS.iter().map(|op| op.doc).collect();
    assert_eq!(
        docs,
        [
            " Add two numbers.\n Refuses the ones this test wants to see fail.",
            " Hand the caller's own text back.",
            " Answer with something JSON has no representation of.",
            "",
            " Greet someone, through a parameter type with its own Serde shape.",
        ]
    );
}

#[test]
fn the_production_operations_are_the_ones_the_vmm_serves() {
    let operations: Vec<(&str, &str)> = crate::OPERATIONS
        .iter()
        .map(|op| (op.name, op.doc))
        .collect();
    assert_eq!(
        operations,
        [
            (
                "Shutdown",
                concat!(
                    " Stop the vmm. Answered once every device has quiesced, so the reply\n",
                    " means shutdown finished rather than shutdown started."
                )
            ),
            ("Status", " What the vmm is currently doing."),
            ("GetInstanceInfo", " Which instance this vmm is."),
        ]
    );
}

#[test]
fn a_request_is_answered_with_the_type_it_declared() {
    let (reply, answer) = reply_channel();
    serve(TestRequest::Add(Add { lhs: 2, rhs: 3 }, reply));
    // Checked rather than asserted: only `Result<i64, AddError>` fits this answer.
    let sum: Result<i64, AddError> = answer.recv().expect("answered");
    assert_eq!(sum, Ok(5));

    let (reply, answer) = reply_channel();
    serve(TestRequest::Echo(Echo { text: "hi".into() }, reply));
    assert_eq!(answer.recv().expect("answered"), "hi");

    let (reply, answer) = reply_channel();
    serve(TestRequest::Opaque(Opaque {}, reply));
    let _: OpaqueValue = answer.recv().expect("answered");

    // An operation with no doc comment is no less a participant.
    let (reply, answer) = reply_channel();
    serve(TestRequest::Noop(Noop {}, reply));
    let _: () = answer.recv().expect("answered");

    let (reply, answer) = reply_channel();
    serve(TestRequest::Greet(Greet { name: "vm".into() }, reply));
    assert_eq!(answer.recv().expect("answered"), "hello vm");
}

#[test]
fn an_operation_that_refuses_answers_with_its_own_error() {
    let (reply, answer) = reply_channel();
    serve(TestRequest::Add(Add { lhs: 1, rhs: -1 }, reply));
    assert_eq!(answer.recv().expect("answered"), Err(AddError::Forbidden));
}

#[test]
fn an_unanswered_request_disconnects_its_caller() {
    let (reply, answer) = reply_channel();
    let request: TestRequest = TestRequest::Echo(Echo { text: "hi".into() }, reply);

    // The vmm went away holding the request, so its channel went with it.
    drop(request);

    assert!(answer.recv().is_err());
}

#[test]
fn a_caller_that_stops_waiting_does_not_cancel_the_operation() {
    let (reply, answer) = reply_channel();
    drop(answer);

    // Answering into a dropped receiver is not something the vmm can act on.
    assert_eq!(
        serve(TestRequest::Echo(Echo { text: "hi".into() }, reply)),
        "Echo"
    );
}

#[test]
fn requests_cross_threads() {
    fn send<T: Send + 'static>() {}
    fn send_and_sync<T: Send + Sync + 'static>() {}

    // A request is handed to the vmm thread, so it has to travel.
    send::<TestRequest>();
    // A command is data, so a caller can hold it from anywhere.
    send_and_sync::<Command>();
}

#[cfg(feature = "wire")]
mod wire;
