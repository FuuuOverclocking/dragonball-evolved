use std::future::Future;
use std::io;

use http::{Request, Response};
use tokio::net::UnixStream;

use crate::proto::{Connection, Incoming};
use crate::{Body, Limits};

pub async fn serve_connection<F, Fut>(stream: UnixStream, service: F) -> io::Result<()>
where
    F: FnMut(Request<Body>) -> Fut,
    Fut: Future<Output = io::Result<Response<Body>>>,
{
    serve_connection_with_limits(stream, Limits::default(), service).await
}

pub async fn serve_connection_with_limits<F, Fut>(
    stream: UnixStream,
    limits: Limits,
    mut service: F,
) -> io::Result<()>
where
    F: FnMut(Request<Body>) -> Fut,
    Fut: Future<Output = io::Result<Response<Body>>>,
{
    let mut connection = Connection::new(stream, limits);
    loop {
        let mut request = Request::new(Body::empty());
        let Some(close) = connection.receive(Incoming::Request(&mut request)).await? else {
            break;
        };
        let (parts, mut body) = service(request).await?.into_parts();
        let close = connection.prepare_response(&parts, &mut body, close)?;
        drop(parts);
        connection.send(&body).await?;
        if close {
            break;
        }
    }
    Ok(())
}
