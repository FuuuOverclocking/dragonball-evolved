//! Snapshot round-trip integration tests.
//!
//! Checkpoint 5: verify that a paused VM can be captured, torn down, rebuilt
//! from the snapshot, and resumed with the guest continuing to execute.
//!
//! Checkpoint 6: verify that block device reactivation works after a rebuild
//! when the transport was in DRIVER_OK state.

#[allow(dead_code)]
#[path = "common/elf.rs"]
mod elf;

use std::path::Path;
use std::time::Duration;

use tokio::time;
use vmm::KvmContext;
use vmm::machine::build;
use vmm::machine::bus::BusDevice;
use vmm::machine::snapshot::{self, rebuild};
use vmm::spec::MachineSpec;

/// Guest payload: infinite loop incrementing eax.
///
/// ```asm
/// .loop:
///   inc eax
///   jmp .loop    ; eb fc = jmp -4 (back to inc)
/// ```
fn inc_eax_payload() -> Vec<u8> {
    vec![
        0xff, 0xc0, // inc eax
        0xeb, 0xfc, // jmp -4 (back to inc)
    ]
}

// ---------------------------------------------------------------------------
// Checkpoint 5: register-increment round-trip (no disks, no serial)
// ---------------------------------------------------------------------------

/// Prove that a snapshot → teardown → rebuild → resume cycle preserves guest
/// execution by verifying that a register-incrementing guest keeps running.
///
/// The guest payload is `inc eax; jmp $-2`, which continuously increments rax.
/// After the first pause, rax = N > 0.  After rebuild + resume + pause, rax
/// should have advanced further, proving the guest survived the round-trip.
#[tokio::test(flavor = "local")]
async fn register_increment_round_trip_on_kvm() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
    let kernel = elf::TempFile::new(&elf::image(&inc_eax_payload(), true));

    // Use MachinePlan + build() for the initial machine so that rebuild() is
    // tested against a machine with the same structure that build() creates.
    use api::boot::KernelConfig;
    use api::machine::ConsoleConfig;
    use vmm::machine::plan::MachinePlan;

    let spec = MachineSpec {
        cpus: 1,
        memory_mib: 16,
        console: ConsoleConfig::None,
        kernel: Some(KernelConfig {
            path: kernel.path().into(),
            cmdline: None,
            initrd: None,
        }),
        disks: Vec::new(),
    };
    let plan = MachinePlan::new(spec.clone(), context.capabilities()).unwrap();
    let mut machine = build::build(plan, &context).unwrap();

    // Resume vCPUs — guest starts executing `inc eax` in a tight loop.
    machine.vcpus.as_ref().unwrap().resume().await.unwrap();

    // Let the guest run long enough to increment eax substantially.
    time::sleep(Duration::from_millis(50)).await;

    // Pause and capture.
    machine.pause().await.unwrap();
    let snap = snapshot::capture(&machine).unwrap();

    // Read rax from the captured snapshot.
    let rax_before = snap.vcpus[0].regs.rax;
    assert!(rax_before > 0, "guest did not execute (rax = {rax_before})");

    // Teardown the original machine — the memory Arc in the snapshot keeps
    // the mmap alive.
    machine.teardown().await.unwrap();

    // Rebuild from the snapshot.
    let mut restored = rebuild(snap, &spec, &context).unwrap();

    // Resume vCPUs — the guest should continue from where it left off.
    restored.vcpus.as_ref().unwrap().resume().await.unwrap();
    time::sleep(Duration::from_millis(50)).await;

    // Pause again and verify rax advanced.
    restored.pause().await.unwrap();
    let snap2 = snapshot::capture(&restored).unwrap();
    let rax_after = snap2.vcpus[0].regs.rax;
    assert!(
        rax_after > rax_before,
        "guest did not advance after rebuild (rax: {rax_before} -> {rax_after})"
    );

    restored.teardown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Checkpoint 6: block device reactivation round-trip
// ---------------------------------------------------------------------------

/// Verify that a block device whose transport was in DRIVER_OK state is
/// correctly reactivated after rebuild.
///
/// This test uses a GuestDriver-style approach: it manually negotiates virtio
/// activation on the initial machine, quiesces the block device, takes a
/// snapshot, tears down, and rebuilds. After rebuild, the block device's
/// activation channel receives the restored queue and can resume I/O.
///
/// Because this tests the rebuild path and not a live guest, we only verify
/// that the rebuilt machine constructs without error, the block device task
/// spawns, and the device manager can cleanly stop.
#[tokio::test(flavor = "local")]
async fn block_reactivation_after_rebuild_on_kvm() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
    let kernel = elf::TempFile::new(&elf::image(&inc_eax_payload(), true));

    use api::block::BlockDevConfig;
    use api::boot::KernelConfig;
    use api::machine::ConsoleConfig;
    use vmm::machine::plan::MachinePlan;

    // Create a small disk image (2 sectors).
    let disk_contents = vec![0xab_u8; 1024];
    let disk = common::TempDisk::new("snap-blk", &disk_contents);

    let spec = MachineSpec {
        cpus: 1,
        memory_mib: 16,
        console: ConsoleConfig::None,
        kernel: Some(KernelConfig {
            path: kernel.path().into(),
            cmdline: None,
            initrd: None,
        }),
        disks: vec![BlockDevConfig {
            id: Some("test-disk".into()),
            path: disk.path().into(),
            read_only: false,
            root: false,
        }],
    };
    let plan = MachinePlan::new(spec.clone(), context.capabilities()).unwrap();
    let mut machine = build::build(plan, &context).unwrap();

    // The guest payload does not actually drive virtio, so the transport stays
    // at status 0 (not activated). To test block reactivation, we need to
    // simulate the guest having negotiated DRIVER_OK. We do this by writing
    // through the MMIO transport bus device to perform the negotiation
    // sequence.
    //
    // virtio-mmio negotiation: status 0→1→3, set features, status 3→11→15,
    // set queue addresses, set queue ready, status = 15 (DRIVER_OK).
    if !machine.mmio_transports.is_empty() {
        let transport = &machine.mmio_transports[0];

        // Write status = ACKNOWLEDGE (1)
        transport.write(0x70, &1u32.to_le_bytes());
        // Write status = ACKNOWLEDGE | DRIVER (3)
        transport.write(0x70, &3u32.to_le_bytes());
        // Select driver_features_sel = 1 (high)
        transport.write(0x24, &1u32.to_le_bytes());
        // Write VERSION_1 bit (bit 0 of high word)
        transport.write(0x20, &1u32.to_le_bytes());
        // Write status = ACKNOWLEDGE | DRIVER | FEATURES_OK (11)
        transport.write(0x70, &11u32.to_le_bytes());

        // Configure the queue: use a small reserved area in guest memory.
        // Queue size
        transport.write(0x38, &16u32.to_le_bytes());
        // desc table low
        transport.write(0x80, &0x4000u32.to_le_bytes());
        // avail ring low
        transport.write(0x90, &0x5000u32.to_le_bytes());
        // used ring low
        transport.write(0xa0, &0x6000u32.to_le_bytes());
        // Queue ready = 1
        transport.write(0x44, &1u32.to_le_bytes());
        // Write status = DRIVER_OK (15)
        transport.write(0x70, &15u32.to_le_bytes());
    }

    // Let the device start processing.
    time::sleep(Duration::from_millis(50)).await;

    // Resume vCPUs to let guest run briefly (it just increments eax).
    machine.vcpus.as_ref().unwrap().resume().await.unwrap();
    time::sleep(Duration::from_millis(20)).await;

    // Pause and capture.
    machine.pause().await.unwrap();
    let snap = snapshot::capture(&machine).unwrap();

    // Verify that the transport snapshot has DRIVER_OK status.
    assert_eq!(
        snap.devices.transports.len(),
        1,
        "expected exactly one transport snapshot"
    );
    assert_eq!(
        snap.devices.transports[0].regs.status, 15,
        "transport should be in DRIVER_OK (15) state"
    );

    // The block quiesce should have captured queue state.
    assert_eq!(
        snap.devices.block_queues.len(),
        1,
        "expected one captured block queue"
    );

    machine.teardown().await.unwrap();

    // Rebuild from the snapshot — this should reactivate the block device
    // with the captured queue state.
    let mut restored = rebuild(snap, &spec, &context).unwrap();

    // Let the rebuilt device tasks settle.
    time::sleep(Duration::from_millis(50)).await;

    // The rebuild succeeded, the device manager has tasks running, and
    // everything can be stopped cleanly.
    assert!(
        restored.devices.running() > 0,
        "no device tasks running after rebuild"
    );

    // Resume vCPUs and let the guest run briefly.
    restored.vcpus.as_ref().unwrap().resume().await.unwrap();
    time::sleep(Duration::from_millis(20)).await;

    // Verify the restored machine can be paused and torn down cleanly.
    restored.pause().await.unwrap();
    restored.teardown().await.unwrap();
}

// Test helper module.
#[allow(dead_code)]
mod common {
    use std::path::{Path, PathBuf};

    pub struct TempDisk {
        path: PathBuf,
    }

    impl TempDisk {
        pub fn new(name: &str, contents: &[u8]) -> Self {
            let path =
                std::env::temp_dir().join(format!("dragonball-test-{}-{name}", std::process::id()));
            std::fs::write(&path, contents).expect("write temp disk");
            Self { path }
        }

        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDisk {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
