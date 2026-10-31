#[path = "common/elf.rs"]
mod elf;

use std::path::Path;
use std::time::Duration;

use api::boot::{KernelConfig, SetKernel};
use api::instance_info::{GetInstanceInfo, VmState};
use api::lifecycle::{Pause, Resume, Shutdown, Start};
use api::machine::{ConsoleConfig, MemorySizeMib, SetConsole, SetMemory, VcpuCount};
use vmm::{KvmContext, ProcessCtx};

fn client() -> vmm::VmmClient {
    vmm::start(ProcessCtx {
        id: "boot-test".into(),
        version: "test".into(),
        kvm_ctx: KvmContext::from_dev_path("/dev/kvm").unwrap(),
        update_logger: |_| Ok(()),
    })
    .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn failed_start_is_retryable_and_running_vm_pauses_resumes_and_stops() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let invalid = elf::TempFile::new(&elf::image(&[0xeb, 0xfe], false));
    let kernel = elf::TempFile::new(&elf::image(&[0xeb, 0xfe], true));
    // SAFETY: Only this test thread's signal mask is changed, and restored after spawning.
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
    // SAFETY: `saved` was initialized by pthread_sigmask.
    assert_eq!(
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &saved, std::ptr::null_mut()) },
        0
    );
    client
        .call_async(SetConsole(ConsoleConfig::None))
        .await
        .unwrap()
        .unwrap();
    client
        .call_async(SetMemory(MemorySizeMib(16)))
        .await
        .unwrap()
        .unwrap();
    client
        .call_async(SetKernel(KernelConfig {
            path: invalid.path().into(),
            cmdline: None,
            initrd: None,
        }))
        .await
        .unwrap()
        .unwrap();
    assert!(
        client
            .call_async(Start)
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("no PVH entry")
    );
    assert_eq!(
        client.call_async(GetInstanceInfo).await.unwrap().state,
        VmState::NotStarted
    );
    client
        .call_async(SetKernel(KernelConfig {
            path: kernel.path().into(),
            cmdline: None,
            initrd: None,
        }))
        .await
        .unwrap()
        .unwrap();
    client.call_async(Start).await.unwrap().unwrap();
    assert_eq!(
        client.call_async(GetInstanceInfo).await.unwrap().state,
        VmState::Running
    );
    assert!(
        client
            .call_async(SetMemory(MemorySizeMib(32)))
            .await
            .unwrap()
            .is_err()
    );
    for _ in 0..10 {
        client.call_async(Pause).await.unwrap().unwrap();
        assert_eq!(
            client.call_async(GetInstanceInfo).await.unwrap().state,
            VmState::Paused
        );
        client.call_async(Pause).await.unwrap().unwrap();
        client.call_async(Resume).await.unwrap().unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), client.call_async(Shutdown))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        client.async_join_vmm_thread().await.unwrap(),
        vmm::VmmExitStatus::Ok
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn guest_reset_stops_a_two_vcpu_machine() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let kernel = elf::TempFile::new(&elf::image(&elf::serial_reset_payload(false), true));
    let client = client();
    client
        .call_async(SetConsole(ConsoleConfig::None))
        .await
        .unwrap()
        .unwrap();
    client
        .call_async(api::machine::SetCpus(VcpuCount(2)))
        .await
        .unwrap()
        .unwrap();
    client
        .call_async(SetMemory(MemorySizeMib(16)))
        .await
        .unwrap()
        .unwrap();
    client
        .call_async(SetKernel(KernelConfig {
            path: kernel.path().into(),
            cmdline: None,
            initrd: None,
        }))
        .await
        .unwrap()
        .unwrap();
    client.call_async(Start).await.unwrap().unwrap();
    assert!(matches!(
        client.async_join_vmm_thread().await.unwrap(),
        vmm::VmmExitStatus::Ok
    ));
}
