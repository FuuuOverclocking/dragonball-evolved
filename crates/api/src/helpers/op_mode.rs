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

pub trait OpMode {
    type Reply<T>;
}

pub struct Request;

impl OpMode for Request {
    type Reply<T> = Reply<T>;
}

pub struct Command;

impl OpMode for Command {
    type Reply<T> = ();
}
