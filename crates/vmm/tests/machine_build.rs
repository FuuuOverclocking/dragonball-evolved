//! A machine that fails to build leaves nothing behind: no fd, no thread and
//! no guest memory mapping.
//!
//! This binary holds a single test, so nothing else in the process opens or
//! closes descriptors or threads while it counts them.

#[path = "common/elf.rs"]
mod elf;

use std::fs::{self, File};
use std::path::Path;

use api::RawVmmOp;
use api::boot::{KernelConfig, SetKernel};
use api::machine::{
    ConsoleConfig, MemoryBacking, MemorySizeMib, SetConsole, SetMemory, SetMemoryBacking,
};
use tokio::runtime::LocalRuntime;
use vmm::KvmContext;
use vmm::machine::{BootDataError, BuildError, MachinePlan, build};
use vmm::spec::MachineSpec;

use self::elf::TempFile;

/// What the process holds that a build could leak.
#[derive(Debug, PartialEq, Eq)]
struct Held {
    fds: usize,
    threads: usize,
    guest_mappings: usize,
}

fn held() -> Held {
    let count = |dir| fs::read_dir(dir).unwrap().count();
    let maps = fs::read_to_string("/proc/self/maps").unwrap();
    Held {
        fds: count("/proc/self/fd"),
        threads: count("/proc/self/task"),
        guest_mappings: maps
            .lines()
            .filter(|line| line.contains("memfd:guest-memory"))
            .count(),
    }
}

fn spec(kernel: &Path, initrd: Option<&Path>) -> MachineSpec {
    [
        SetKernel(KernelConfig {
            path: kernel.to_owned(),
            cmdline: None,
            initrd: initrd.map(Path::to_owned),
        })
        .into_vmm_command(),
        SetMemory(MemorySizeMib(16)).into_vmm_command(),
        SetMemoryBacking(MemoryBacking::Shared).into_vmm_command(),
        SetConsole(ConsoleConfig::None).into_vmm_command(),
    ]
    .into_iter()
    .try_fold(MachineSpec::default(), |spec, command| spec.apply(command))
    .unwrap()
}

fn try_build(context: &KvmContext, spec: &MachineSpec) -> BuildError {
    let (plan, policy) = MachinePlan::new(spec, context.capabilities()).unwrap();
    match build(plan, policy, context) {
        Ok(_) => panic!("the build succeeded"),
        Err(error) => error,
    }
}

#[test]
fn failed_builds_leave_nothing_behind_on_kvm() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
    // Building needs the runtime its device tasks would run on.
    LocalRuntime::new()
        .unwrap()
        .block_on(failed_builds_leave_nothing_behind(&context));
}

async fn failed_builds_leave_nothing_behind(context: &KvmContext) {
    let payload = elf::serial_reset_payload(false);
    let pvh = TempFile::new(&elf::image(&payload, true));
    let not_pvh = TempFile::new(&elf::image(&payload, false));
    let missing = pvh.path().with_extension("missing");
    // Larger than all of low RAM; sparse, so it costs no disk.
    let large = TempFile::new(b"");
    File::options()
        .write(true)
        .open(large.path())
        .unwrap()
        .set_len(16 << 20)
        .unwrap();
    let baseline = held();

    let error = try_build(context, &spec(&missing, None));
    assert!(
        matches!(&error, BuildError::OpenKernel { path, .. } if *path == missing),
        "{error:?}"
    );
    assert_eq!(held(), baseline);

    let error = try_build(context, &spec(not_pvh.path(), None));
    assert!(matches!(error, BuildError::NoPvhEntry(_)), "{error:?}");
    assert_eq!(held(), baseline);

    let error = try_build(context, &spec(pvh.path(), Some(large.path())));
    assert!(
        matches!(
            error,
            BuildError::Boot(BootDataError::InitrdTooLarge { size, .. }) if size == 16 << 20
        ),
        "{error:?}"
    );
    assert_eq!(held(), baseline);

    // The same context still builds a machine, which holds what failures did not.
    let (machine, _events) = {
        let (plan, policy) =
            MachinePlan::new(&spec(pvh.path(), None), context.capabilities()).unwrap();
        build(plan, policy, context).unwrap()
    };
    let built = held();
    assert_eq!(built.guest_mappings, 1);
    assert_eq!(built.threads, baseline.threads + 1);
    machine.teardown().await.expect("the machine tears down");
    assert_eq!(held().guest_mappings, 0);
}
