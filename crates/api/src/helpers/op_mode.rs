#[cfg(feature = "request")]
use crate::helpers::reply::Reply;

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
