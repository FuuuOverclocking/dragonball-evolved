//! Linux UDS HTTP/1.1 RPC: sequential Content-Length messages, inline bodies and first-byte SCM_RIGHTS.
#![forbid(unsafe_op_in_unsafe_fn)]

#[cfg(not(target_os = "linux"))]
compile_error!("hyper-uds currently requires Linux for atomic CLOEXEC descriptor reception");

mod body;
pub mod client;
mod proto;
pub mod server;
mod transport;

pub use body::{Body, Fds};
pub use client::Client;
pub use http::{HeaderMap, Method, Request, Response, StatusCode, Uri, Version, header};
pub use server::{serve, serve_connection};
pub use tokio::net::{UnixListener, UnixStream};

// Linux limits a single SCM_RIGHTS message to SCM_MAX_FD descriptors.
pub const MAX_FDS: usize = 253;
pub const MAX_HEADERS: usize = 64;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_header_bytes: usize,
    pub max_body_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_header_bytes: 8192,
            max_body_bytes: 1024 * 1024,
        }
    }
}

fn invalid(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}
