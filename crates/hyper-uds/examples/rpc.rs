use std::fs::File;
use std::io::{self, Read};

use hyper_uds::{Body, Client, Request, Response, UnixStream, serve_connection};

fn run_server(server: UnixStream) -> tokio::task::JoinHandle<Result<(), io::Error>> {
    let service = async |mut request: Request<Body>| {
        let fd = request
            .body_mut()
            .fds_mut()
            .take_named("input")
            .ok_or_else(|| io::Error::other("missing input descriptor"))?;
        let mut file = File::from(fd);

        let mut byte = [1];
        file.read_exact(&mut byte)?;
        assert_eq!(byte, [0]);

        let mut body = Body::from(br#"{"jsonrpc":"2.0","result":"ok","id":1}"#);
        body.fds_mut().push_named("output", file.into())?;

        Ok(Response::new(body))
    };
    tokio::spawn(serve_connection(server, service))
}

async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
    let (client, server) = UnixStream::pair()?;
    let server = run_server(server);

    let mut client = Client::new(client);
    let mut body = Body::from(br#"{"jsonrpc":"2.0","method":"check","id":1}"#);
    body.fds_mut()
        .push_named("input", File::open("/dev/zero")?.into())?;
    let request = Request::builder()
        .method("POST")
        .uri("/rpc")
        .header("content-type", "application/json")
        .header("connection", "close")
        .body(body)?;
    let mut response = client.send_request(request).await?;
    assert_eq!(response.body().fds().index_of("output"), Some(0));
    let fd = response.body_mut().fds_mut().take(0).unwrap();
    let mut file = File::from(fd);
    let mut byte = [1];
    file.read_exact(&mut byte)?;
    assert_eq!(byte, [0]);
    println!(
        "{}: {} (returned FD verified)",
        response.status(),
        std::str::from_utf8(response.body().as_bytes())?
    );
    server.await??;

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()?
        .block_on(async_main())
}
