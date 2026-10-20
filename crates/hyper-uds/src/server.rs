use std::future::{Future, pending};
use std::io;

use http::{Request, Response};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::proto::{Connection, Incoming};
use crate::{Body, Limits};

pub async fn serve<F, Fut, S>(listener: UnixListener, service: F, shutdown: S) -> io::Result<()>
where
    F: FnMut(Request<Body>) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = io::Result<Response<Body>>> + Send + 'static,
    S: Future<Output = ()>,
{
    serve_with_limits(listener, Limits::default(), service, shutdown).await
}

pub async fn serve_with_limits<F, Fut, S>(
    listener: UnixListener,
    limits: Limits,
    service: F,
    shutdown: S,
) -> io::Result<()>
where
    F: FnMut(Request<Body>) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = io::Result<Response<Body>>> + Send + 'static,
    S: Future<Output = ()>,
{
    tokio::pin!(shutdown);
    let (stop, stopping) = watch::channel(());
    let mut connections = JoinSet::new();
    let result = loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break Ok(()),
            // A malformed peer must not shut down the listener.
            _ = connections.join_next(), if !connections.is_empty() => {}
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => break Err(error),
                };
                let mut stopping = stopping.clone();
                connections.spawn(serve_connection_with_shutdown(
                    stream,
                    limits,
                    service.clone(),
                    async move { let _ = stopping.changed().await; },
                ));
            }
        }
    };
    drop(listener);
    drop(stop);
    while connections.join_next().await.is_some() {}
    result
}

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
    service: F,
) -> io::Result<()>
where
    F: FnMut(Request<Body>) -> Fut,
    Fut: Future<Output = io::Result<Response<Body>>>,
{
    serve_connection_with_shutdown(stream, limits, service, pending()).await
}

pub async fn serve_connection_with_shutdown<F, Fut, S>(
    stream: UnixStream,
    limits: Limits,
    mut service: F,
    shutdown: S,
) -> io::Result<()>
where
    F: FnMut(Request<Body>) -> Fut,
    Fut: Future<Output = io::Result<Response<Body>>>,
    S: Future<Output = ()>,
{
    tokio::pin!(shutdown);
    let mut connection = Connection::new(stream, limits);
    loop {
        let mut request = Request::new(Body::empty());
        let Some(mut close) = connection
            .receive_with_shutdown(Incoming::Request(&mut request), &mut shutdown)
            .await?
        else {
            break;
        };
        let response = service(request);
        tokio::pin!(response);
        let response = tokio::select! {
            biased;
            _ = &mut shutdown => {
                close = true;
                response.await?
            }
            response = &mut response => response?,
        };
        let (parts, mut body) = response.into_parts();
        let close = connection.prepare_response(&parts, &mut body, close)?;
        drop(parts);
        connection.send(&body).await?;
        if close {
            break;
        }
    }
    Ok(())
}
