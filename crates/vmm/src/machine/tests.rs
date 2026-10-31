//! Machine-level tests on KVM, booting the mini PVH guest from
//! `tests/common/elf.rs`.

#[path = "../../tests/common/elf.rs"]
mod elf;

use std::path::Path;
use std::time::Duration;

use api::boot::{KernelConfig, SetKernel};
use api::machine::{
    ConsoleConfig, MemoryBacking, MemorySizeMib, SetConsole, SetCpus, SetMemory, SetMemoryBacking,
    VcpuCount,
};
use api::{RawVmmOp, VmmCommand};
use linux_loader::loader::elf::start_info::XEN_HVM_START_MAGIC_VALUE;
use tokio::time;
use vm_memory::{Bytes, GuestAddress};

use self::elf::TempFile;
use super::build::build_with;
use super::platform::{Captured, ConsoleBackend};
use super::*;
use crate::arch::x86_64::layout::PVH_INFO_START;
use crate::devices::runtime::Response;
use crate::kvm_context::KvmContext;
use crate::memory::policy::BackingKind;
use crate::memory::store::MemoryView;
use crate::spec::MachineSpec;

const TIMEOUT: Duration = Duration::from_secs(5);
const BOOT_OK: &[u8] = b"DBE_BOOT_OK\n";
/// Where the looping guest counts its iterations.
const COUNTER: u64 = 0x8000;
/// `inc dword [COUNTER]; jmp back`, forever.
const LOOP: [u8; 8] = [0xff, 0x05, 0x00, 0x80, 0x00, 0x00, 0xeb, 0xf8];

fn kvm() -> Option<KvmContext> {
    Path::new("/dev/kvm")
        .exists()
        .then(|| KvmContext::from_dev_path("/dev/kvm").unwrap())
}

fn spec(kernel: &TempFile, initrd: Option<&TempFile>, more: Vec<VmmCommand>) -> MachineSpec {
    let kernel = SetKernel(KernelConfig {
        path: kernel.path().to_owned(),
        cmdline: None,
        initrd: initrd.map(|initrd| initrd.path().to_owned()),
    });
    [kernel.into_vmm_command()]
        .into_iter()
        .chain(more)
        .try_fold(MachineSpec::default(), |spec, command| spec.apply(command))
        .unwrap()
}

/// Builds `spec` with COM1's output captured instead of on stdout.
fn build(context: &KvmContext, spec: &MachineSpec) -> (PausedMachine, MachineEvents, Captured) {
    let (plan, policy) = MachinePlan::new(spec, context.capabilities()).unwrap();
    let output = Captured::default();
    let (machine, events) = build_with(
        plan,
        policy,
        context,
        ConsoleBackend::Captured(output.clone()),
    )
    .unwrap();
    (machine, events, output)
}

async fn next_event(events: &mut MachineEvents) -> MachineEvent {
    time::timeout(TIMEOUT, events.machine.recv())
        .await
        .expect("no machine event in time")
        .expect("the machine events closed")
}

/// Whether the looping guest counts on.
async fn advances(memory: &MemoryView) -> bool {
    let read = || memory.read_obj::<u32>(GuestAddress(COUNTER)).unwrap();
    let before = read();
    time::timeout(TIMEOUT, async {
        while read() == before {
            time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .is_ok()
}

/// Whether the looping guest stays where it is for a while.
async fn stays(memory: &MemoryView) -> bool {
    let read = || memory.read_obj::<u32>(GuestAddress(COUNTER)).unwrap();
    let before = read();
    time::sleep(Duration::from_millis(20)).await;
    read() == before
}

/// After teardown nothing can send: every event left is drained, then the
/// stream ends.
async fn closed(events: &mut MachineEvents) {
    time::timeout(TIMEOUT, async {
        while events.machine.recv().await.is_some() {}
    })
    .await
    .expect("the machine events stayed open after teardown");
}

#[tokio::test(flavor = "local")]
async fn a_built_machine_boots_the_mini_guest_to_its_reset_on_kvm() {
    let Some(context) = kvm() else { return };
    let kernel = TempFile::new(&elf::image(&elf::serial_reset_payload(false), true));
    let (machine, mut events, output) = build(&context, &spec(&kernel, None, vec![]));
    assert_eq!(machine.vcpu_count(), 1);
    assert!(machine.plan().has_com1());
    // Built parked: nothing has run, and every device starts quiesced.
    assert_eq!(
        machine.quiesced(),
        [QuiescedDevice {
            name: "console-input".into(),
            response: Response::Replied(()),
        }]
    );
    assert_eq!(
        machine
            .memory()
            .read_obj::<u32>(GuestAddress(PVH_INFO_START.0))
            .unwrap(),
        XEN_HVM_START_MAGIC_VALUE
    );
    assert!(output.0.lock().unwrap().is_empty());

    let machine = machine.resume().await.map_err(|(_, error)| error).unwrap();
    assert_eq!(next_event(&mut events).await, MachineEvent::GuestReset);
    assert_eq!(*output.0.lock().unwrap(), BOOT_OK);
    let layout = machine.plan().memory().clone();
    let memory = machine.teardown().await.unwrap();
    assert_eq!(memory.layout(), &layout);
    closed(&mut events).await;
}

#[tokio::test(flavor = "local")]
async fn two_vcpus_tear_down_after_the_reset_and_hand_back_the_memory_on_kvm() {
    let Some(context) = kvm() else { return };
    let kernel = TempFile::new(&elf::image(&elf::serial_reset_payload(false), true));
    let spec = spec(
        &kernel,
        None,
        vec![
            SetCpus(VcpuCount(2)).into_vmm_command(),
            SetMemoryBacking(MemoryBacking::Shared).into_vmm_command(),
            SetConsole(ConsoleConfig::None).into_vmm_command(),
        ],
    );
    let (machine, mut events, output) = build(&context, &spec);
    assert_eq!(machine.vcpu_count(), 2);
    // Without a console the guest has no COM1, and the platform no device task.
    assert!(!machine.plan().has_com1());
    assert!(machine.quiesced().is_empty());

    let machine = machine.resume().await.map_err(|(_, error)| error).unwrap();
    assert_eq!(next_event(&mut events).await, MachineEvent::GuestReset);
    assert!(output.0.lock().unwrap().is_empty());
    // The application processor still waits for its INIT; teardown stops it all the same.
    let memory = machine.teardown().await.unwrap();
    assert_eq!(memory.policy().kind, BackingKind::SharedMemfd);
    assert_eq!(
        memory
            .view()
            .read_obj::<u32>(GuestAddress(PVH_INFO_START.0))
            .unwrap(),
        XEN_HVM_START_MAGIC_VALUE
    );
    closed(&mut events).await;
}

#[tokio::test(flavor = "local")]
async fn the_guest_finds_its_initrd_through_the_module_list_on_kvm() {
    let Some(context) = kvm() else { return };
    let mut code = vec![
        0x83, 0x7b, 0x0c, 1, // cmp dword [ebx + nr_modules], 1
        0x74, 2, 0x0f, 0x0b, // je ok; ud2
        0x8b, 0x7b, 0x10, // mov edi, [ebx + modlist_paddr]
        0x8b, 0x0f, // mov ecx, [edi]: the initrd's address
        0x80, 0x39, b'I', // cmp byte [ecx], 'I'
        0x74, 2, 0x0f, 0x0b, // je ok; ud2
    ];
    code.extend(elf::serial_reset_payload(false));
    let kernel = TempFile::new(&elf::image(&code, true));
    let initrd = TempFile::new(b"INITRD_TEST");
    let spec = spec(
        &kernel,
        Some(&initrd),
        vec![SetMemory(MemorySizeMib(16)).into_vmm_command()],
    );
    let (machine, mut events, output) = build(&context, &spec);
    // At the top of low RAM, on a page.
    let placed = (16 << 20) - 0x1000;
    let mut contents = [0; 11];
    machine
        .memory()
        .read_slice(&mut contents, GuestAddress(placed))
        .unwrap();
    assert_eq!(&contents, b"INITRD_TEST");

    let machine = machine.resume().await.map_err(|(_, error)| error).unwrap();
    assert_eq!(next_event(&mut events).await, MachineEvent::GuestReset);
    assert_eq!(*output.0.lock().unwrap(), BOOT_OK);
    machine.teardown().await.unwrap();
}

#[tokio::test(flavor = "local")]
async fn pause_and_resume_round_trip_the_typestate_on_kvm() {
    let Some(context) = kvm() else { return };
    let kernel = TempFile::new(&elf::image(&LOOP, true));
    let spec = spec(
        &kernel,
        None,
        vec![SetCpus(VcpuCount(2)).into_vmm_command()],
    );
    let (paused, mut events, _output) = build(&context, &spec);
    assert!(stays(paused.memory()).await, "a parked guest ran");

    let running = paused.resume().await.map_err(|(_, error)| error).unwrap();
    assert!(advances(running.memory()).await, "the guest did not run");
    let paused = running.pause().await.map_err(|(_, error)| error).unwrap();
    assert!(stays(paused.memory()).await, "the guest ran while paused");
    assert_eq!(
        paused.quiesced(),
        [QuiescedDevice {
            name: "console-input".into(),
            response: Response::Replied(()),
        }]
    );

    let running = paused.resume().await.map_err(|(_, error)| error).unwrap();
    assert!(
        advances(running.memory()).await,
        "the guest did not run again"
    );
    let paused = running.pause().await.map_err(|(_, error)| error).unwrap();
    assert!(stays(paused.memory()).await);
    assert!(
        events.machine.try_recv().is_err(),
        "the guest reported an event"
    );
    paused.teardown().await.unwrap();
    closed(&mut events).await;
}

#[tokio::test(flavor = "local")]
async fn teardown_works_from_both_states_on_kvm() {
    let Some(context) = kvm() else { return };
    let kernel = TempFile::new(&elf::image(&LOOP, true));
    let spec = spec(&kernel, None, vec![]);

    // Never resumed.
    let (paused, mut events, _) = build(&context, &spec);
    let layout = paused.plan().memory().clone();
    assert_eq!(paused.teardown().await.unwrap().layout(), &layout);
    closed(&mut events).await;

    // Running a guest that never exits by itself.
    let (paused, mut events, _) = build(&context, &spec);
    let running = paused.resume().await.map_err(|(_, error)| error).unwrap();
    assert!(advances(running.memory()).await);
    let memory = running.teardown().await.unwrap();
    // The guest's work survives in the memory handed back.
    assert_ne!(
        memory
            .view()
            .read_obj::<u32>(GuestAddress(COUNTER))
            .unwrap(),
        0
    );
    closed(&mut events).await;
}
