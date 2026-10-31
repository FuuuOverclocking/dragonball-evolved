#[path = "../crates/vmm/tests/common/elf.rs"]
mod elf;

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run_config(config: &Path, limit: Duration) -> Output {
    let child = Command::new(env!("CARGO_BIN_EXE_dragonball-evolved"))
        .args(["--config", config.to_str().unwrap()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = ChildGuard(child);
    let mut stdout = child.0.stdout.take().unwrap();
    let mut stderr = child.0.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.0.kill().unwrap();
            child.0.wait().unwrap();
            panic!(
                "guest did not exit: {}",
                String::from_utf8_lossy(&err.join().unwrap())
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Output {
        status,
        stdout: out.join().unwrap(),
        stderr: err.join().unwrap(),
    }
}

#[test]
#[ignore = "P4: machine start is being rebuilt in crates/vmm"]
fn configuration_autostarts_pvh_serial_and_virtio_block() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let kernel = elf::TempFile::new(&elf::image(&elf::serial_reset_payload(true), true));
    let disk = elf::TempFile::new(&[0xab; 512]);
    let config = elf::TempFile::new(format!(
        "{{\"machine\":{{\"cpus\":1,\"memory_mib\":16,\"kernel\":{{\"path\":\"{}\"}},\"disks\":[{{\"path\":\"{}\",\"read_only\":true,\"root\":true}}]}}}}",
        kernel.path().display(), disk.path().display()
    ).as_bytes());
    let output = run_config(config.path(), Duration::from_secs(5));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("DBE_BOOT_OK"),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "P4: machine start is being rebuilt in crates/vmm"]
fn pvh_module_list_points_to_loaded_initrd() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let mut code = vec![
        0x83, 0x7b, 0x0c, 1, 0x74, 2, 0x0f, 0x0b, 0x8b, 0x7b, 0x10, 0x8b, 0x0f, 0x80, 0x39, b'I',
        0x74, 2, 0x0f, 0x0b,
    ];
    code.extend(elf::serial_reset_payload(false));
    let kernel = elf::TempFile::new(&elf::image(&code, true));
    let initrd = elf::TempFile::new(b"INITRD_TEST");
    let config = elf::TempFile::new(
        &serde_json::to_vec(&serde_json::json!({ "machine": {
            "memory_mib": 16, "kernel": { "path": kernel.path(), "initrd": initrd.path() }
        }}))
        .unwrap(),
    );
    let output = run_config(config.path(), Duration::from_secs(5));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("DBE_BOOT_OK"));
}

#[test]
#[ignore = "P4: machine start is being rebuilt in crates/vmm"]
fn configuration_boots_a_guest_on_shared_memory() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let kernel = elf::TempFile::new(&elf::image(&elf::serial_reset_payload(false), true));
    let config = elf::TempFile::new(
        &serde_json::to_vec(&serde_json::json!({ "machine": {
            "memory_mib": 16,
            "memory_backing": "shared",
            "kernel": { "path": kernel.path() }
        }}))
        .unwrap(),
    );
    let output = run_config(config.path(), Duration::from_secs(5));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("DBE_BOOT_OK"));
}

#[test]
#[ignore = "requires DBE_KERNEL and optionally DBE_INITRD/DBE_ROOTFS; unavailable until P5 rebuilds machine start and virtio-blk"]
fn linux_pvh_smoke_boot() {
    let kernel = std::env::var_os("DBE_KERNEL").expect("set DBE_KERNEL to a PVH vmlinux");
    let mut machine = serde_json::json!({ "memory_mib": 512, "kernel": { "path": std::path::PathBuf::from(kernel), "cmdline": std::env::var("DBE_CMDLINE").ok(), "initrd": std::env::var_os("DBE_INITRD").map(std::path::PathBuf::from) } });
    if let Some(path) = std::env::var_os("DBE_ROOTFS") {
        machine["disks"] = serde_json::json!([{ "path": std::path::PathBuf::from(path), "read_only": true, "root": true }]);
    }
    let config = elf::TempFile::new(
        &serde_json::to_vec(&serde_json::json!({ "machine": machine })).unwrap(),
    );
    let output = run_config(config.path(), Duration::from_secs(60));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let console = String::from_utf8_lossy(&output.stdout);
    assert!(console.contains("Linux version"), "{console}");
    assert!(
        console.contains("DBE_LINUX_OK"),
        "init must emit DBE_LINUX_OK before reboot: {console}"
    );
}

struct ApiProcess {
    child: ChildGuard,
    client: hyper_uds::Client,
    socket: std::path::PathBuf,
    _marker: elf::TempFile,
}

impl ApiProcess {
    async fn spawn() -> Self {
        let marker = elf::TempFile::new(b"");
        let socket = marker.path().with_extension("sock");
        let child = Command::new(env!("CARGO_BIN_EXE_dragonball-evolved"))
            .args(["--api-sock", socket.to_str().unwrap()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut child = ChildGuard(child);
        let client = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match hyper_uds::Client::connect(&socket).await {
                    Ok(client) => break client,
                    Err(_) => {
                        assert!(
                            child.0.try_wait().unwrap().is_none(),
                            "API process exited before binding"
                        );
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }
            }
        })
        .await
        .unwrap();
        Self {
            child,
            client,
            socket,
            _marker: marker,
        }
    }

    async fn request(
        &mut self,
        method: &str,
        path: &str,
        body: serde_json::Value,
    ) -> (hyper_uds::StatusCode, serde_json::Value) {
        let request = hyper_uds::Request::builder()
            .method(method)
            .uri(path)
            .body(hyper_uds::Body::json(&body).unwrap())
            .unwrap();
        let response =
            tokio::time::timeout(Duration::from_secs(5), self.client.send_request(request))
                .await
                .unwrap()
                .unwrap();
        let body = serde_json::from_slice(response.body().as_bytes()).unwrap();
        (response.status(), body)
    }

    /// Hangs up, then waits for the process to exit and remove its socket.
    async fn wait_for_exit(self) -> std::process::ExitStatus {
        let Self {
            mut child,
            client,
            socket,
            _marker,
        } = self;
        drop(client);
        let status = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert!(!socket.exists());
        status
    }
}

#[tokio::test(flavor = "current_thread")]
async fn rest_api_configures_and_shuts_down_before_start() {
    use hyper_uds::StatusCode;
    use serde_json::{Value, json};
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let mut api = ApiProcess::spawn().await;
    let disk = json!({ "path": "/test/disk", "root": true });
    for (method, path, body, status) in [
        ("PUT", "/v1/machine/console", json!("none"), StatusCode::OK),
        ("PUT", "/v1/machine/memory-mib", json!(16), StatusCode::OK),
        (
            "PUT",
            "/v1/machine/cpus",
            json!(0),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            "PUT",
            "/v1/machine/kernel",
            json!({ "path": "/test/kernel" }),
            StatusCode::OK,
        ),
        ("PUT", "/v1/disks", disk.clone(), StatusCode::OK),
        ("PUT", "/v1/disks", disk, StatusCode::INTERNAL_SERVER_ERROR),
        (
            "PUT",
            "/v1/actions/pause",
            Value::Null,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ] {
        let (actual, reply) = api.request(method, path, body).await;
        assert_eq!(actual, status, "{method} {path}: {reply}");
    }

    let (status, reply) = api.request("PUT", "/v1/actions/start", Value::Null).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        reply["error"]
            .as_str()
            .unwrap()
            .contains("not supported yet"),
        "{reply}"
    );
    let (status, info) = api.request("GET", "/v1/", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info["state"], "not_started");
    let (status, reply) = api
        .request("PUT", "/v1/actions/shutdown", Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert!(api.wait_for_exit().await.success());
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "P4: machine start is being rebuilt in crates/vmm"]
async fn rest_api_configures_pauses_and_shuts_down_a_real_vm() {
    use hyper_uds::StatusCode;
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let kernel = elf::TempFile::new(&elf::image(&[0xeb, 0xfe], true));
    let mut api = ApiProcess::spawn().await;
    for (method, path, body, status) in [
        ("GET", "/v1/", serde_json::Value::Null, StatusCode::OK),
        (
            "PUT",
            "/v1/machine/console",
            serde_json::json!("none"),
            StatusCode::OK,
        ),
        (
            "PUT",
            "/v1/machine/memory-mib",
            serde_json::json!(16),
            StatusCode::OK,
        ),
        (
            "PUT",
            "/v1/machine/kernel",
            serde_json::json!({ "path": kernel.path() }),
            StatusCode::OK,
        ),
        (
            "PUT",
            "/v1/actions/start",
            serde_json::Value::Null,
            StatusCode::OK,
        ),
        (
            "PUT",
            "/v1/actions/pause",
            serde_json::Value::Null,
            StatusCode::OK,
        ),
        ("GET", "/v1/", serde_json::Value::Null, StatusCode::OK),
        (
            "PUT",
            "/v1/actions/resume",
            serde_json::Value::Null,
            StatusCode::OK,
        ),
        (
            "PUT",
            "/v1/machine/cpus",
            serde_json::json!(2),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            "PUT",
            "/v1/actions/shutdown",
            serde_json::Value::Null,
            StatusCode::OK,
        ),
    ] {
        let (actual, reply) = api.request(method, path, body).await;
        assert_eq!(actual, status, "{path}: {reply}");
        if method == "GET" {
            assert!(matches!(
                reply["state"].as_str(),
                Some("not_started" | "paused")
            ));
        }
    }
    api.wait_for_exit().await;
}
