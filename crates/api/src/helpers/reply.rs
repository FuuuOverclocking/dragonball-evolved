#[cfg(feature = "serde")]
use std::marker::PhantomData;

#[cfg(feature = "serde")]
use crate::helpers::fd::{Fds, SplitFd};

pub struct Reply<T>(oneshot::Sender<T>);

impl<T> From<oneshot::Sender<T>> for Reply<T> {
    fn from(sender: oneshot::Sender<T>) -> Self {
        Self(sender)
    }
}

impl<T> Reply<T> {
    pub fn send(self, value: T) {
        let _ = self.0.send(value);
    }
}

#[cfg(feature = "serde")]
#[derive(Debug)]
pub struct SerializedReply {
    pub value: serde_json::Value,
    pub fds: Fds,
}

/// Distinguishes an explicit operation rejection from failure to deliver its reply.
#[cfg(feature = "serde")]
#[derive(Debug)]
pub enum SerializedReplyError {
    /// The operation's diagnostic has already been flattened at the API boundary.
    Operation(crate::ApiError),
    /// The reply sender disappeared before delivering a value.
    Disconnected(oneshot::RecvError),
    /// Splitting descriptors or serializing the reply failed.
    Encoding(Box<dyn std::error::Error + Send + Sync>),
}

#[cfg(feature = "serde")]
impl std::fmt::Display for SerializedReplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Operation(error) => std::fmt::Display::fmt(error, f),
            Self::Disconnected(error) => std::fmt::Display::fmt(error, f),
            Self::Encoding(error) => std::fmt::Display::fmt(error, f),
        }
    }
}

#[cfg(feature = "serde")]
impl std::error::Error for SerializedReplyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Operation(_) => None,
            Self::Disconnected(error) => Some(error),
            Self::Encoding(error) => Some(error.as_ref()),
        }
    }
}

#[cfg(feature = "serde")]
impl From<serde_json::Error> for SerializedReplyError {
    fn from(error: serde_json::Error) -> Self {
        Self::Encoding(error.into())
    }
}

#[cfg(feature = "serde")]
pub type SerializedReplyWaiter =
    std::pin::Pin<Box<dyn Future<Output = Result<SerializedReply, SerializedReplyError>> + Send>>;

#[cfg(feature = "serde")]
pub(crate) fn into_request<P, T: Send + 'static, R>(
    parameters: P,
    request: impl FnOnce(P, Reply<T>) -> R,
    serialize: fn(T) -> Result<SerializedReply, SerializedReplyError>,
) -> (R, SerializedReplyWaiter) {
    let (sender, receiver) = oneshot::channel();
    let request = request(parameters, sender.into());
    let waiter = Box::pin(async move {
        let value = receiver.await.map_err(SerializedReplyError::Disconnected)?;
        serialize(value)
    });
    (request, waiter)
}

#[cfg(feature = "serde")]
pub(crate) trait SerializeReply<T> {
    fn serialize_reply(self, value: T) -> Result<SerializedReply, SerializedReplyError>;
}

// Autoref selects the ordinary-reply fallback without overlapping ApiResult.
#[cfg(feature = "serde")]
impl<T: serde::Serialize + SplitFd> SerializeReply<T> for &PhantomData<T> {
    fn serialize_reply(self, mut value: T) -> Result<SerializedReply, SerializedReplyError> {
        let fds = value
            .split_fd()
            .map_err(|error| SerializedReplyError::Encoding(error.into()))?;
        Ok(SerializedReply {
            value: serde_json::to_value(value).map_err(SerializedReplyError::from)?,
            fds,
        })
    }
}

#[cfg(feature = "serde")]
impl<T: serde::Serialize + SplitFd> SerializeReply<crate::ApiResult<T>>
    for PhantomData<crate::ApiResult<T>>
{
    fn serialize_reply(
        self,
        value: crate::ApiResult<T>,
    ) -> Result<SerializedReply, SerializedReplyError> {
        (&PhantomData::<T>).serialize_reply(value.map_err(SerializedReplyError::Operation)?)
    }
}
