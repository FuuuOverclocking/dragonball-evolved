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

#[cfg(test)]
mod tests {
    #[test]
    fn command_does_not_need_a_reply_channel() {
        let command: crate::VmmCommand = crate::VmmCommand::AddDisk(Default::default(), ());
        assert!(matches!(command, crate::VmmCommand::AddDisk(_, ())));
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
