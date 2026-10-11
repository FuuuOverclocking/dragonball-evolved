use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::File;
use std::hint::black_box;
use std::io;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use hyper_uds::{Body, Client, HeaderMap, Request, Response, UnixStream, Uri, serve_connection};

struct CountingAllocator;
static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn count_start() {
    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
}

fn count_stop() -> usize {
    COUNTING.store(false, Ordering::Relaxed);
    ALLOCATIONS.load(Ordering::Relaxed)
}

async fn roundtrip(client: &mut Client, bytes: &[u8], fd: &mut Option<OwnedFd>) -> io::Result<()> {
    let mut body = Body::from(bytes);
    if let Some(descriptor) = fd.take() {
        body.fds_mut().push_named("file", descriptor)?;
    }
    let request = Request::builder()
        .method("POST")
        .uri(Uri::from_static("/rpc"))
        .header("content-type", "application/json")
        .body(body)
        .unwrap();
    let mut response = client.send_request(request).await?;
    assert_eq!(response.body().as_bytes(), bytes);
    if !response.body().fds().is_empty() {
        *fd = response.body_mut().fds_mut().take_named("file");
    }
    black_box(response);
    Ok(())
}

async fn rpc_bench(size: usize, with_fd: bool, iterations: usize, profile: bool) -> io::Result<()> {
    let (client, server) = UnixStream::pair()?;
    let server = tokio::spawn(serve_connection(server, |request| async {
        Ok(Response::new(request.into_body()))
    }));
    let mut client = Client::new(client);
    let mut bytes = vec![b' '; size];
    if size >= 2 {
        bytes[..2].copy_from_slice(b"{}");
    }
    let mut fd = if with_fd {
        Some(File::open("/dev/null")?.into())
    } else {
        None
    };
    for _ in 0..1000 {
        roundtrip(&mut client, &bytes, &mut fd).await?;
    }
    if profile {
        let start = Instant::now();
        for _ in 0..iterations {
            roundtrip(&mut client, &bytes, &mut fd).await?;
        }
        println!(
            "{size} B fd={with_fd} iterations={iterations} mean={:.3} us",
            start.elapsed().as_secs_f64() * 1e6 / iterations as f64
        );
        drop(client);
        server.await??;
        return Ok(());
    }
    let mut samples = Vec::with_capacity(iterations);
    let start = Instant::now();
    for _ in 0..iterations {
        let sample = Instant::now();
        roundtrip(&mut client, &bytes, &mut fd).await?;
        samples.push(sample.elapsed().as_nanos() as u64);
    }
    let elapsed = start.elapsed();
    count_start();
    for _ in 0..100 {
        roundtrip(&mut client, &bytes, &mut fd).await?;
    }
    let allocations = count_stop();
    samples.sort_unstable();
    println!(
        "{size:5} B fd={with_fd:<5} p50={:7.2} us p99={:7.2} us {:9.0} rpc/s alloc/rpc={:.1}",
        samples[iterations / 2] as f64 / 1000.0,
        samples[iterations * 99 / 100] as f64 / 1000.0,
        iterations as f64 / elapsed.as_secs_f64(),
        allocations as f64 / 100.0
    );
    drop(client);
    server.await??;
    Ok(())
}

fn parser_bench(iterations: usize) {
    let raw = b"POST /rpc HTTP/1.1\r\nhost: localhost\r\ncontent-length: 2\r\ncontent-type: application/json\r\n\r\n{}";
    let parse = || {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut request = httparse::Request::new(&mut headers);
        black_box(request.parse(black_box(raw)).unwrap());
    };
    let materialize = || {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut parsed = httparse::Request::new(&mut headers);
        parsed.parse(black_box(raw)).unwrap();
        let mut request = Request::new(Body::from(b"{}"));
        *request.method_mut() = parsed.method.unwrap().parse().unwrap();
        *request.uri_mut() = parsed.path.unwrap().parse().unwrap();
        *request.headers_mut() = HeaderMap::with_capacity(parsed.headers.len());
        for header in parsed.headers {
            request.headers_mut().append(
                hyper_uds::header::HeaderName::from_bytes(header.name.as_bytes()).unwrap(),
                hyper_uds::header::HeaderValue::from_bytes(header.value).unwrap(),
            );
        }
        black_box(request);
    };
    for (name, operation) in [
        ("httparse", &parse as &dyn Fn()),
        ("parse + http + inline Body", &materialize as &dyn Fn()),
    ] {
        for _ in 0..1000 {
            operation();
        }
        let start = Instant::now();
        for _ in 0..iterations {
            operation();
        }
        let ns = start.elapsed().as_nanos() as f64 / iterations as f64;
        count_start();
        for _ in 0..100 {
            operation();
        }
        let allocations = count_stop();
        println!(
            "{name}: {ns:.0} ns/message, {:.1} allocations/message",
            allocations as f64 / 100.0
        );
    }
}

fn main() -> io::Result<()> {
    let iterations = std::env::var("ITERATIONS")
        .ok()
        .map(|s| s.parse::<usize>().expect("ITERATIONS must be an integer"))
        .unwrap_or(10_000)
        .max(100);
    println!(
        "Linux socketpair, current-thread runtime, warm persistent connection; timing and allocation runs are separate"
    );
    let profile_bytes = std::env::var("PROFILE_BYTES").ok().map(|s| {
        s.parse::<usize>()
            .expect("PROFILE_BYTES must be an integer")
    });
    let profile_fd = std::env::var("PROFILE_FD")
        .ok()
        .map(|s| s.parse::<bool>().expect("PROFILE_FD must be true or false"))
        .unwrap_or(false);
    if profile_bytes.is_none() {
        parser_bench(iterations * 10);
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()?
        .block_on(async {
            if let Some(size) = profile_bytes {
                return rpc_bench(size, profile_fd, iterations, true).await;
            }
            for size in [0, 64, 256, 511, 4096] {
                rpc_bench(size, false, iterations, false).await?;
            }
            for size in [64, 511] {
                rpc_bench(size, true, iterations, false).await?;
            }
            Ok(())
        })
}
