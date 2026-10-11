use std::io;
use std::path::Path;

use http::{Request, Response};
use tokio::net::UnixStream;

use crate::proto::{Connection, Incoming};
use crate::{Body, Limits};

pub struct Client {
    connection: Option<Connection>,
}

impl Client {
    pub async fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self::new(UnixStream::connect(path).await?))
    }

    pub fn new(stream: UnixStream) -> Self {
        Self::with_limits(stream, Limits::default())
    }

    pub fn with_limits(stream: UnixStream, limits: Limits) -> Self {
        Self {
            connection: Some(Connection::new(stream, limits)),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.connection.is_none()
    }

    /// Cancellation after polling closes the connection instead of reusing a partial exchange.
    pub async fn send_request(&mut self, request: Request<Body>) -> io::Result<Response<Body>> {
        let mut connection = self
            .connection
            .take()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))?;
        let (parts, mut body) = request.into_parts();
        let close = match connection.prepare_request(&parts, &mut body) {
            Ok(close) => close,
            Err(error) => {
                self.connection = Some(connection);
                return Err(error);
            }
        };
        drop(parts);
        connection.send(&body).await?;
        drop(body);
        let mut response = Response::new(Body::empty());
        let response_close = connection
            .receive(Incoming::Response(&mut response))
            .await?
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        if !close && !response_close {
            self.connection = Some(connection);
        }
        Ok(response)
    }
}
