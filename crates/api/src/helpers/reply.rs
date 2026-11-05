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

#[cfg(all(test, feature = "serde"))]
mod tests {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::task::{Context, Poll, Waker};

    use super::*;
    use crate::helpers::fd::{MergeFd, SplitFd};
    use crate::instance_info::{GetInstanceInfo, InstanceInfo};
    use crate::live_upgrade::{HandoverState, LiveUpgradeState};
    use crate::{ApiError, ApiResult, RawVmmOp, VmmRequest};

    fn ready(mut waiter: SerializedReplyWaiter) -> Result<SerializedReply, SerializedReplyError> {
        let Poll::Ready(result) = waiter
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        else {
            panic!("reply was not ready");
        };
        result
    }

    #[test]
    fn handover_reply_preserves_json_and_descriptor() {
        let op = crate::metadata::OPERATIONS
            .iter()
            .find(|op| op.name == "HandoverState")
            .unwrap();
        let command = (op.parse)(serde_json::json!({}).into()).unwrap();
        let (request, waiter) = command.into_request();
        let VmmRequest::HandoverState(_, reply) = request else {
            panic!("expected HandoverState");
        };
        let (socket, _peer) = UnixStream::pair().unwrap();
        let raw_fd = socket.as_raw_fd();
        reply.send(Ok(LiveUpgradeState {
            test_fd: Some(socket.into()),
        }));

        let SerializedReply { value, fds } = ready(waiter).unwrap();
        assert_eq!(value, serde_json::json!({}));
        assert_eq!(fds.len(), 1);
        assert_eq!(fds[0].as_raw_fd(), raw_fd);
        let mut slots: Vec<_> = fds.into_iter().map(Some).collect();
        let mut decoded: LiveUpgradeState = serde_json::from_value(value).unwrap();
        decoded.merge_fd(&mut slots).unwrap();
        assert!(slots.iter().all(Option::is_none));
        assert_eq!(decoded.test_fd.as_ref().unwrap().as_raw_fd(), raw_fd);
    }

    #[test]
    fn ordinary_and_unit_replies_have_no_descriptors() {
        let (request, waiter) = GetInstanceInfo.into_vmm_command().into_request();
        let VmmRequest::GetInstanceInfo(_, reply) = request else {
            panic!("expected GetInstanceInfo");
        };
        let info = InstanceInfo::default();
        let value = serde_json::to_value(&info).unwrap();
        reply.send(info);
        let serialized = ready(waiter).unwrap();
        assert_eq!(serialized.value, value);
        assert!(serialized.fds.is_empty());

        let (request, waiter) = crate::block::AddDisk(crate::block::BlockDevConfig {
            id: None,
            path: "/test/disk".into(),
            read_only: false,
            root: false,
        })
        .into_vmm_command()
        .into_request();
        let VmmRequest::AddDisk(_, reply) = request else {
            panic!("expected AddDisk");
        };
        reply.send(Ok(()));
        let serialized = ready(waiter).unwrap();
        assert_eq!(serialized.value, serde_json::Value::Null);
        assert!(serialized.fds.is_empty());
    }

    #[test]
    fn failed_operations_and_disconnected_replies_remain_errors() {
        let (request, waiter) = HandoverState {}.into_vmm_command().into_request();
        let VmmRequest::HandoverState(_, reply) = request else {
            panic!("expected HandoverState");
        };
        reply.send(Err(ApiError::msg("handover failed")));
        let SerializedReplyError::Operation(error) = ready(waiter).unwrap_err() else {
            panic!("expected an operation rejection");
        };
        assert_eq!(error, ApiError::msg("handover failed"));

        let (request, waiter) = HandoverState {}.into_vmm_command().into_request();
        drop(request);
        assert!(matches!(
            ready(waiter),
            Err(SerializedReplyError::Disconnected(_))
        ));
    }

    #[test]
    fn failures_with_identical_diagnostics_keep_distinct_types() {
        let message = "machine is not started";
        let operation = SerializedReplyError::Operation(ApiError::msg(message));
        assert!(std::error::Error::source(&operation).is_none());
        let SerializedReplyError::Operation(error) = operation else {
            panic!("expected operation rejection");
        };
        assert_eq!(error, ApiError::msg(message));

        let encoding = SerializedReplyError::Encoding(ApiError::msg(message).into());
        assert!(std::error::Error::source(&encoding).is_some());
        assert!(matches!(encoding, SerializedReplyError::Encoding(_)));

        let disconnected = SerializedReplyError::Disconnected(oneshot::RecvError);
        assert!(std::error::Error::source(&disconnected).is_some());
        assert!(matches!(
            disconnected,
            SerializedReplyError::Disconnected(_)
        ));
    }

    #[test]
    fn handover_reply_propagates_split_errors() {
        let (request, waiter) = HandoverState {}.into_vmm_command().into_request();
        let VmmRequest::HandoverState(_, reply) = request else {
            panic!("expected HandoverState");
        };
        reply.send(Ok(LiveUpgradeState { test_fd: None }));
        let SerializedReplyError::Encoding(error) = ready(waiter).unwrap_err() else {
            panic!("expected a descriptor encoding failure");
        };
        assert_eq!(error.to_string(), "test_fd is not bound");
    }

    #[test]
    fn split_failure_skips_serialization_and_closes_partial_resources() {
        struct FailingState(Fds);

        impl SplitFd for FailingState {
            fn split_fd(&mut self) -> ApiResult<Fds> {
                drop(self.0.pop());
                Err(ApiError::msg("partial split failed"))
            }
        }

        impl serde::Serialize for FailingState {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                panic!("must not serialize after split failure");
            }
        }

        for wrapped in [false, true] {
            let (first, mut first_peer) = UnixStream::pair().unwrap();
            let (second, mut second_peer) = UnixStream::pair().unwrap();
            let state = FailingState(vec![first.into(), second.into()]);
            let result = if wrapped {
                PhantomData::<ApiResult<FailingState>>.serialize_reply(Ok(state))
            } else {
                PhantomData::<FailingState>.serialize_reply(state)
            };
            let SerializedReplyError::Encoding(error) = result.unwrap_err() else {
                panic!("expected a descriptor encoding failure");
            };
            assert_eq!(error.to_string(), "partial split failed");
            for peer in [&mut first_peer, &mut second_peer] {
                peer.set_nonblocking(true).unwrap();
                assert_eq!(peer.read(&mut [0]).unwrap(), 0);
            }
        }
    }

    #[test]
    fn dropping_waiter_closes_resources_before_or_after_reply() {
        for reply_first in [false, true] {
            let (request, waiter) = HandoverState {}.into_vmm_command().into_request();
            let VmmRequest::HandoverState(_, reply) = request else {
                panic!("expected HandoverState");
            };
            let (socket, mut peer) = UnixStream::pair().unwrap();
            let state = LiveUpgradeState {
                test_fd: Some(socket.into()),
            };
            if reply_first {
                reply.send(Ok(state));
                drop(waiter);
            } else {
                drop(waiter);
                reply.send(Ok(state));
            }
            peer.set_nonblocking(true).unwrap();
            assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        }
    }

    #[test]
    fn serialization_failure_closes_split_descriptors() {
        struct FailingState(LiveUpgradeState);

        impl SplitFd for FailingState {
            fn split_fd(&mut self) -> ApiResult<Fds> {
                self.0.split_fd()
            }
        }

        impl serde::Serialize for FailingState {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("serialization failed"))
            }
        }

        let (socket, mut peer) = UnixStream::pair().unwrap();
        let state = FailingState(LiveUpgradeState {
            test_fd: Some(socket.into()),
        });
        let SerializedReplyError::Encoding(error) = PhantomData::<FailingState>
            .serialize_reply(state)
            .unwrap_err()
        else {
            panic!("expected a JSON encoding failure");
        };
        assert_eq!(error.to_string(), "serialization failed");
        assert!(error.is::<serde_json::Error>());
        peer.set_nonblocking(true).unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
}
