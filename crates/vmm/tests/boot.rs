#[path = "common/elf.rs"]
mod elf;

use std::path::Path;
use std::time::Duration;

use api::RawVmmOp;
use api::boot::{KernelConfig, SetKernel};
use api::instance_info::{GetInstanceInfo, VmState};
use api::lifecycle::{Pause, Resume, Shutdown, Start};
use api::machine::{ConsoleConfig, MemorySizeMib, SetConsole, SetCpus, SetMemory, VcpuCount};
use vmm::{KvmContext, ProcessCtx, VmmClient, VmmExitStatus};

const TIMEOUT: Duration = Duration::from_secs(10);

fn client() -> VmmClient {
    vmm::start(ProcessCtx {
        id: "boot-test".into(),
        version: "test".into(),
        kvm_ctx: KvmContext::from_dev_path("/dev/kvm").unwrap(),
        update_logger: |_| Ok(()),
    })
    .unwrap()
}

async fn call<Op: api::RawVmmOp>(client: &VmmClient, op: Op) -> Op::Response {
    tokio::time::timeout(TIMEOUT, client.call_async(op))
        .await
        .expect("the request did not finish")
        .unwrap()
}

fn kernel(file: &elf::TempFile) -> SetKernel {
    SetKernel(KernelConfig {
        path: file.path().into(),
        cmdline: None,
        initrd: None,
    })
}

async fn joined(client: &VmmClient) -> VmmExitStatus {
    tokio::time::timeout(TIMEOUT, client.async_join_vmm_thread())
        .await
        .expect("the VMM did not exit")
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn failed_start_is_retryable_and_running_vm_pauses_resumes_and_stops() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let invalid = elf::TempFile::new(&elf::image(&[0xeb, 0xfe], false));
    let valid = elf::TempFile::new(&elf::image(&[0xeb, 0xfe], true));
    // Exercise the inherited-mask case through vmm-main, not just a vCPU helper.
    // SAFETY: only this test thread's mask is changed, and restored after spawning.
    let saved = unsafe {
        let mut mask = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        let mut saved = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        libc::sigemptyset(mask.as_mut_ptr());
        libc::sigaddset(mask.as_mut_ptr(), libc::SIGRTMIN());
        assert_eq!(
            libc::pthread_sigmask(libc::SIG_BLOCK, mask.as_ptr(), saved.as_mut_ptr()),
            0
        );
        saved.assume_init()
    };
    let client = client();
    // SAFETY: saved was initialized by pthread_sigmask above.
    assert_eq!(
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &saved, std::ptr::null_mut()) },
        0
    );
    call(&client, SetConsole(ConsoleConfig::None))
        .await
        .unwrap();
    call(&client, SetCpus(VcpuCount(2))).await.unwrap();
    call(&client, SetMemory(MemorySizeMib(16))).await.unwrap();
    // Plan failure is retryable too, not just a loader failure.
    assert!(
        call(&client, Start)
            .await
            .unwrap_err()
            .to_string()
            .contains("no kernel")
    );
    assert_eq!(
        call(&client, GetInstanceInfo).await.state,
        VmState::NotStarted
    );
    call(&client, kernel(&invalid)).await.unwrap();
    assert!(
        call(&client, Start)
            .await
            .unwrap_err()
            .to_string()
            .contains("no PVH entry")
    );
    assert_eq!(
        call(&client, GetInstanceInfo).await.state,
        VmState::NotStarted
    );
    call(&client, kernel(&valid)).await.unwrap();
    call(&client, Start).await.unwrap();
    assert_eq!(call(&client, GetInstanceInfo).await.state, VmState::Running);
    assert!(call(&client, SetMemory(MemorySizeMib(32))).await.is_err());
    assert!(call(&client, Start).await.is_err());
    // Resume is idempotent when already running.
    call(&client, Resume).await.unwrap();
    for _ in 0..10 {
        call(&client, Pause).await.unwrap();
        assert_eq!(call(&client, GetInstanceInfo).await.state, VmState::Paused);
        call(&client, Pause).await.unwrap();
        call(&client, Resume).await.unwrap();
        assert_eq!(call(&client, GetInstanceInfo).await.state, VmState::Running);
    }
    call(&client, Shutdown).await.unwrap();
    assert_eq!(joined(&client).await, VmmExitStatus::Ok);
}

#[tokio::test(flavor = "current_thread")]
async fn guest_reset_stops_a_two_vcpu_machine() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let file = elf::TempFile::new(&elf::image(&elf::serial_reset_payload(false), true));
    let client = client();
    call(&client, SetConsole(ConsoleConfig::None))
        .await
        .unwrap();
    call(&client, SetCpus(VcpuCount(2))).await.unwrap();
    call(&client, SetMemory(MemorySizeMib(16))).await.unwrap();
    call(&client, kernel(&file)).await.unwrap();
    call(&client, Start).await.unwrap();
    assert_eq!(joined(&client).await, VmmExitStatus::Ok);
}

#[tokio::test(flavor = "current_thread")]
async fn guest_reset_with_public_pause_resume_requests_exits_ok() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    // Finite guest work varies the scheduling opportunities without sleeps or
    // requiring a particular race outcome. The private main-loop test covers
    // reset observation deterministically inside an in-flight Pause.
    for iterations in [100_000u32, 1_000_000, 10_000_000] {
        let mut code = vec![0xb9]; // mov ecx, iterations
        code.extend_from_slice(&iterations.to_le_bytes());
        code.extend_from_slice(&[0xe2, 0xfe]); // loop back until ecx == 0
        code.extend(elf::serial_reset_payload(false));
        let file = elf::TempFile::new(&elf::image(&code, true));
        let client = client();
        call(&client, SetConsole(ConsoleConfig::None))
            .await
            .unwrap();
        call(&client, SetCpus(VcpuCount(2))).await.unwrap();
        call(&client, SetMemory(MemorySizeMib(16))).await.unwrap();
        call(&client, kernel(&file)).await.unwrap();
        call(&client, Start).await.unwrap();
        tokio::time::timeout(TIMEOUT, async {
            'requests: for _ in 0..1_000 {
                for (request, reply) in [Pause.into_vmm_request(), Resume.into_vmm_request()] {
                    // Reset may close admission, drop a queued reply, or cause
                    // the transition to report an exited vCPU. The exit status,
                    // not a racing lifecycle reply, is the regression oracle.
                    if client.send(request).is_err() || !matches!(reply.await, Ok(Ok(()))) {
                        break 'requests;
                    }
                }
            }
            // Every completed pair leaves the guest running; no host Shutdown
            // can accidentally turn a lost reset event into a successful exit.
            assert_eq!(joined(&client).await, VmmExitStatus::Ok);
        })
        .await
        .expect("pause/resume requests or guest reset did not finish");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_tears_down_a_paused_machine() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let file = elf::TempFile::new(&elf::image(&[0xeb, 0xfe], true));
    let client = client();
    call(&client, SetConsole(ConsoleConfig::None))
        .await
        .unwrap();
    call(&client, kernel(&file)).await.unwrap();
    call(&client, Start).await.unwrap();
    call(&client, Pause).await.unwrap();
    call(&client, Shutdown).await.unwrap();
    assert_eq!(joined(&client).await, VmmExitStatus::Ok);
}
