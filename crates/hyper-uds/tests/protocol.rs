use std::fs::File;
use std::future::ready;
use std::io::{self, Read, Write};
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::thread;
use std::time::Duration;

use hyper_uds::{Body, Request, Response, StatusCode, UnixStream, serve_connection};

const VALID_REQUEST: &[u8] = b"GET / HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n";

fn spawn_server<H>(mut service: H) -> (StdUnixStream, thread::JoinHandle<io::Result<()>>)
where
    H: FnMut(Request<Body>) -> io::Result<Response<Body>> + Send + 'static,
{
    let (client, server) = StdUnixStream::pair().unwrap();
    server.set_nonblocking(true).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let handle = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()?
            .block_on(async move {
                let stream = UnixStream::from_std(server)?;
                serve_connection(stream, move |request| ready(service(request))).await
            })
    });
    (client, handle)
}

fn exchange<H>(request: &[u8], service: H) -> (io::Result<()>, Vec<u8>)
where
    H: FnMut(Request<Body>) -> io::Result<Response<Body>> + Send + 'static,
{
    let (mut client, handle) = spawn_server(service);
    client.write_all(request).unwrap();
    client.shutdown(std::net::Shutdown::Write).unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).unwrap();
    (handle.join().unwrap(), response)
}

fn assert_rejected(request: &[u8]) {
    let (result, response) = exchange(request, |_| Ok(Response::new(Body::empty())));
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert!(response.is_empty());
}

fn receive_response(raw: &[u8]) -> io::Result<Response<Body>> {
    let (client, mut server) = StdUnixStream::pair().unwrap();
    client.set_nonblocking(true).unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let raw = raw.to_vec();
    let peer = thread::spawn(move || -> io::Result<()> {
        let mut request = Vec::new();
        let mut buffer = [0; 512];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let n = server.read(&mut buffer)?;
            if n == 0 {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            request.extend_from_slice(&buffer[..n]);
        }
        server.write_all(&raw)?;
        server.shutdown(std::net::Shutdown::Write)
    });
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()?
        .block_on(async move {
            let stream = UnixStream::from_std(client)?;
            let mut client = hyper_uds::Client::new(stream);
            let request = Request::builder().uri("/").body(Body::empty()).unwrap();
            client.send_request(request).await
        });
    peer.join().unwrap()?;
    result
}

fn send_with_fds(stream: &StdUnixStream, bytes: &[u8], fds: &[RawFd]) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let control_len = unsafe { libc::CMSG_SPACE(size_of_val(fds) as u32) as usize };
    let mut control = vec![0usize; control_len.div_ceil(size_of::<usize>())];
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control_len;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&message);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(size_of_val(fds) as u32) as usize;
        std::ptr::copy_nonoverlapping(
            fds.as_ptr().cast::<u8>(),
            libc::CMSG_DATA(cmsg),
            size_of_val(fds),
        );
    }
    let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &message, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    if sent as usize != bytes.len() {
        return Err(io::Error::from(io::ErrorKind::WriteZero));
    }
    Ok(())
}

#[test]
fn request_without_content_length_has_empty_body() {
    let (result, response) = exchange(VALID_REQUEST, |request| {
        assert!(request.body().is_empty());
        Ok(Response::new(Body::empty()))
    });
    result.unwrap();
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert!(
        response
            .windows(19)
            .any(|line| line == b"content-length: 0\r\n")
    );
}

#[test]
fn rejects_ambiguous_or_unsupported_request_framing() {
    for request in [
        &b"GET / HTTP/1.1\r\nconnection: close\r\n\r\n"[..],
        &b"GET / HTTP/1.1\r\nhost: a\r\nhost: b\r\nconnection: close\r\n\r\n"[..],
        &b"GET / HTTP/1.1\r\nhost: \t\r\nconnection: close\r\n\r\n"[..],
        &b"POST / HTTP/1.1\r\nhost: a\r\ncontent-length: 0\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"[..],
        &b"POST / HTTP/1.1\r\nhost: a\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n0\r\n\r\n"[..],
        &b"GET / HTTP/1.1\r\nhost: a\r\nte: trailers\r\nconnection: close\r\n\r\n"[..],
        &b"GET / HTTP/1.1\r\nhost: a\r\nexpect: 100-continue\r\nconnection: close\r\n\r\n"[..],
        &b"GET / HTTP/1.1\r\nhost: a\r\nconnection: upgrade\r\nupgrade: websocket\r\n\r\n"[..],
        &b"GET / HTTP/1.1\r\nhost: a\r\ntrailer: digest\r\nconnection: close\r\n\r\n"[..],
        &b"GET / HTTP/1.1\r\nhost: a\r\nx-fd: one\r\nx-fd: two\r\nconnection: close\r\n\r\n"[..],
    ] {
        assert_rejected(request);
    }
}

#[test]
fn rejects_unsupported_version_target_and_pipelining() {
    for request in [
        &b"GET / HTTP/1.0\r\nhost: a\r\n\r\n"[..],
        &b"HEAD / HTTP/1.1\r\nhost: a\r\n\r\n"[..],
        &b"CONNECT localhost HTTP/1.1\r\nhost: a\r\n\r\n"[..],
        &b"GET http://localhost/ HTTP/1.1\r\nhost: a\r\n\r\n"[..],
        &b"GET / HTTP/1.1\r\nhost: a\r\ncontent-length: 0\r\n\r\nGET / HTTP/1.1\r\nhost: a\r\n\r\n"
            [..],
    ] {
        assert_rejected(request);
    }
}

#[test]
fn enforces_incoming_response_body_rules() {
    let error = receive_response(b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n").unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);

    let error = receive_response(
        b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);

    let response = receive_response(
        b"HTTP/1.1 304 Not Modified\r\ncontent-length: 123\r\nconnection: close\r\n\r\n",
    )
    .unwrap();
    assert!(response.body().is_empty());
    assert_eq!(response.headers()["content-length"], "123");

    let error =
        receive_response(b"HTTP/1.1 205 Reset Content\r\nconnection: close\r\n\r\n").unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);

    let response = receive_response(
        b"HTTP/1.1 205 Reset Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .unwrap();
    assert!(response.body().is_empty());
}

#[test]
fn rejects_ambiguous_or_unsupported_response_framing() {
    for response in [
        &b"HTTP/1.0 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"[..],
        &b"HTTP/1.1 101 Switching Protocols\r\nconnection: close\r\n\r\n"[..],
        &b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            [..],
        &b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n0\r\n\r\n"[..],
        &b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\nx"[..],
    ] {
        let error = receive_response(response).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}

#[test]
fn enforces_outgoing_response_body_rules() {
    let (result, _) = exchange(VALID_REQUEST, |_| {
        Ok(Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::from("invalid"))
            .unwrap())
    });
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);

    let (result, _) = exchange(VALID_REQUEST, |_| {
        Ok(Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header("content-length", "0")
            .body(Body::empty())
            .unwrap())
    });
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);

    let (result, response) = exchange(VALID_REQUEST, |_| {
        Ok(Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header("content-length", "123")
            .body(Body::empty())
            .unwrap())
    });
    result.unwrap();
    assert!(
        response
            .windows(21)
            .any(|line| line == b"content-length: 123\r\n")
    );

    let (result, response) = exchange(VALID_REQUEST, |_| {
        Ok(Response::builder()
            .status(StatusCode::RESET_CONTENT)
            .body(Body::empty())
            .unwrap())
    });
    result.unwrap();
    assert!(
        response
            .windows(19)
            .any(|line| line == b"content-length: 0\r\n")
    );
}

#[test]
fn binds_exact_fd_names_and_rejects_mismatches() {
    let file = File::open("/dev/null").unwrap();
    let request = b"GET / HTTP/1.1\r\nhost: localhost\r\nx-fd: input\r\nconnection: close\r\n\r\n";
    let (client, handle) = spawn_server(|request| {
        assert_eq!(request.body().fds().len(), 1);
        assert!(request.body().fds().get_named("input").is_some());
        Ok(Response::new(Body::empty()))
    });
    send_with_fds(&client, request, &[file.as_raw_fd()]).unwrap();
    client.shutdown(std::net::Shutdown::Write).unwrap();
    handle.join().unwrap().unwrap();

    for (names, count) in [("input, input", 2), ("input, other", 1), ("input", 2)] {
        let request = format!(
            "GET / HTTP/1.1\r\nhost: localhost\r\nx-fd: {names}\r\nconnection: close\r\n\r\n"
        );
        let fds = vec![file.as_raw_fd(); count];
        let (client, handle) = spawn_server(|_| Ok(Response::new(Body::empty())));
        send_with_fds(&client, request.as_bytes(), &fds).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(
            handle.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}

#[test]
fn fd_limit_rejects_an_extra_descriptor_without_leaking_it() {
    for named in [false, true] {
        let mut fds = hyper_uds::Fds::default();
        for index in 0..hyper_uds::MAX_FDS {
            let fd = File::open("/dev/null").unwrap().into();
            let slot = if named {
                fds.push_named(&format!("fd{index}"), fd)
            } else {
                fds.push(fd)
            };
            assert_eq!(slot.unwrap(), index);
        }
        let (socket, mut peer) = StdUnixStream::pair().unwrap();
        let error = if named {
            fds.push_named("extra", socket.into())
        } else {
            fds.push(socket.into())
        }
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fds.len(), hyper_uds::MAX_FDS);
        peer.set_nonblocking(true).unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
}

#[test]
fn roundtrips_fd_batches_up_to_the_linux_limit() {
    let (client, handle) = spawn_server(|request| {
        let expected_count: usize = request.headers()["fd-count"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(request.body().fds().len(), expected_count);
        for index in 0..expected_count {
            let fd = request.body().fds().get(index).unwrap();
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            assert_ne!(flags, -1);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
        Ok(Response::new(request.into_body()))
    });
    client.set_nonblocking(true).unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap()
        .block_on(async move {
            let mut client = hyper_uds::Client::new(UnixStream::from_std(client).unwrap());
            for named in [false, true] {
                for count in [0, 1, 16, 17, hyper_uds::MAX_FDS] {
                    let mut body = Body::from("fd roundtrip");
                    for index in 0..count {
                        let file = File::open(if index % 2 == 0 {
                            "/dev/zero"
                        } else {
                            "/dev/null"
                        })
                        .unwrap();
                        if named {
                            body.fds_mut()
                                .push_named(&format!("fd{index}"), file.into())
                                .unwrap();
                        } else {
                            body.fds_mut().push(file.into()).unwrap();
                        }
                    }
                    let request = Request::builder()
                        .method("POST")
                        .uri("/")
                        .header("fd-count", count.to_string())
                        .body(body)
                        .unwrap();
                    let mut response = client.send_request(request).await.unwrap();
                    assert_eq!(response.body().as_bytes(), b"fd roundtrip");
                    assert_eq!(response.body().fds().len(), count);
                    for index in 0..count {
                        let name = format!("fd{index}");
                        assert_eq!(
                            response.body().fds().name(index),
                            if named { Some(name.as_str()) } else { None }
                        );
                        let fd = response.body_mut().fds_mut().take(index).unwrap();
                        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
                        assert_ne!(flags, -1);
                        assert_ne!(flags & libc::FD_CLOEXEC, 0);
                        let mut byte = [1];
                        let n = File::from(fd).read(&mut byte).unwrap();
                        if index % 2 == 0 {
                            assert_eq!((n, byte), (1, [0]));
                        } else {
                            assert_eq!(n, 0);
                        }
                    }
                }
            }
        });
    handle.join().unwrap().unwrap();
}
