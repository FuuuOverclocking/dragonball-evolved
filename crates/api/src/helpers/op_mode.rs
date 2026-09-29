#[cfg(all(feature = "request", feature = "serde"))]
use std::marker::PhantomData;

pub trait OpMode {
    type Reply<T>;
}

#[cfg(feature = "request")]
pub struct Request;

#[cfg(feature = "request")]
impl OpMode for Request {
    type Reply<T> = Reply<T>;
}

pub struct Command;

impl OpMode for Command {
    type Reply<T> = ();
}

#[cfg(feature = "request")]
pub struct Reply<T>(oneshot::Sender<T>);

#[cfg(feature = "request")]
impl<T> From<oneshot::Sender<T>> for Reply<T> {
    fn from(sender: oneshot::Sender<T>) -> Self {
        Self(sender)
    }
}

#[cfg(feature = "request")]
impl<T> Reply<T> {
    pub fn send(self, value: T) {
        let _ = self.0.send(value);
    }
}

#[cfg(all(feature = "request", feature = "serde"))]
pub type ReplyWaiter =
    std::pin::Pin<Box<dyn Future<Output = crate::ApiResult<serde_json::Value>> + Send>>;

#[cfg(all(feature = "request", feature = "serde"))]
pub(crate) fn into_request<P, T: Send + 'static, R>(
    parameters: P,
    request: impl FnOnce(P, Reply<T>) -> R,
    serialize: fn(T) -> crate::ApiResult<serde_json::Value>,
) -> (R, ReplyWaiter) {
    let (sender, receiver) = oneshot::channel();
    let request = request(parameters, sender.into());
    let waiter = Box::pin(async move { serialize(receiver.await?) });
    (request, waiter)
}

#[cfg(all(feature = "request", feature = "serde"))]
pub(crate) trait SerializeReply<T> {
    fn serialize_reply(self, value: T) -> crate::ApiResult<serde_json::Value>;
}

// Autoref selects the ordinary-reply fallback without overlapping ApiResult.
#[cfg(all(feature = "request", feature = "serde"))]
impl<T: serde::Serialize> SerializeReply<T> for &PhantomData<T> {
    fn serialize_reply(self, value: T) -> crate::ApiResult<serde_json::Value> {
        Ok(serde_json::to_value(value)?)
    }
}

#[cfg(all(feature = "request", feature = "serde"))]
impl<T: serde::Serialize> SerializeReply<crate::ApiResult<T>> for PhantomData<crate::ApiResult<T>> {
    fn serialize_reply(self, value: crate::ApiResult<T>) -> crate::ApiResult<serde_json::Value> {
        Ok(serde_json::to_value(value?)?)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn command_does_not_need_a_reply_channel() {
        let command: crate::VmmCommand = crate::VmmCommand::AddDisk(Default::default(), ());
        assert!(matches!(command.clone(), crate::VmmCommand::AddDisk(_, ())));
    }

    #[cfg(feature = "request")]
    #[test]
    fn request_delivers_its_typed_reply() {
        let (sender, receiver) = oneshot::channel();
        let request: crate::VmmRequest =
            crate::VmmRequest::AddDisk(Default::default(), sender.into());
        let crate::VmmRequest::AddDisk(_, reply) = request else {
            unreachable!();
        };
        reply.send(Ok(()));
        assert_eq!(receiver.recv().unwrap(), Ok(()));
    }

    #[cfg(feature = "request")]
    #[test]
    fn reply_tolerates_a_dropped_receiver() {
        let (sender, receiver) = oneshot::channel();
        drop(receiver);
        crate::Reply::from(sender).send(());
    }
}
