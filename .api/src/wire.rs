//! Where an operation's own reply type stops mattering.
//!
//! A caller that serves every operation with the same code cannot hold one reply
//! type per operation, so here -- and only here -- a reply is erased, to JSON
//! rather than `dyn Any`.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use serde::Serialize;
use serde_json::Value;

use crate::Answer;

/// Why an erased reply did not arrive as JSON.
#[derive(Debug)]
pub enum ReplyError {
    /// The vmm dropped the reply channel without answering.
    Disconnected,
    /// The answer arrived but has no JSON representation.
    Serialization(serde_json::Error),
}

impl fmt::Display for ReplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => f.write_str("the vmm closed the reply channel without answering"),
            Self::Serialization(e) => write!(f, "serialize the vmm's answer: {e}"),
        }
    }
}

impl std::error::Error for ReplyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Disconnected => None,
            Self::Serialization(e) => Some(e),
        }
    }
}

/// A prepared operation's answer, erased to JSON: awaiting is the only thing a
/// caller can do with it.
///
/// Dropping a waiter is not a cancellation: the vmm keeps running the operation
/// and answers into a channel nobody holds.
pub type ReplyWaiter = Pin<Box<dyn Future<Output = Result<Value, ReplyError>> + Send>>;

/// Erase one operation's reply type.
///
/// Generated code calls this once per operation, which is what keeps the single
/// boxing at the boundary instead of in every handler.
pub(crate) fn waiter<T>(answer: Answer<T>) -> ReplyWaiter
where
    T: Serialize + Send + 'static,
{
    Box::pin(async move {
        let value = answer.await.map_err(|_| ReplyError::Disconnected)?;
        serde_json::to_value(value).map_err(ReplyError::Serialization)
    })
}
