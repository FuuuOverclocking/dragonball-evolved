//! Snapshot round-trip integration tests.
//!
//! Checkpoint 5: verify that a paused VM can be captured, torn down, rebuilt
//! from the snapshot, and resumed with the guest continuing to execute.
//!
//! Checkpoint 6: verify that block device reactivation works after a rebuild
//! for the disks whose device had an active session, where the plan put them.

mod common;
#[allow(dead_code)]
#[path = "common/elf.rs"]
mod elf;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::guest::{
    AVAIL_RING, DESC_TABLE, GuestDriver, SECTOR_SIZE, Segment, USED_RING, VIRTIO_BLK_S_OK,
    VIRTIO_BLK_T_IN,
};
use common::{TempDisk, settle, wait_until};
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

/// Negotiate a virtio-mmio transport up to DRIVER_OK the way a guest driver
/// would: status 0→1→3, set features, status 3→11, set the queue addresses,
/// set queue ready, status 11→15.
fn negotiate_driver_ok(
    transport: &vmm::devices::virtio::mmio::MmioTransport,
    desc: u64,
    avail: u64,
    used: u64,
) {
    for (offset, value) in [
        // status = ACKNOWLEDGE, then ACKNOWLEDGE | DRIVER
        (0x70, 1u32),
        (0x70, 3),
        // driver_features_sel = 1 (high word), then VERSION_1 (its bit 0)
        (0x24, 1),
        (0x20, 1),
        // status = ACKNOWLEDGE | DRIVER | FEATURES_OK
        (0x70, 11),
        // queue size, then the low words of desc table, avail ring, used ring
        (0x38, 16),
        (0x80, desc as u32),
        (0x90, avail as u32),
        (0xa0, used as u32),
        // queue ready, then status = DRIVER_OK
        (0x44, 1),
        (0x70, 15),
    ] {
        transport.write(offset, &value.to_le_bytes());
    }
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
    let disk = TempDisk::new("snap-blk", &disk_contents);

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
    negotiate_driver_ok(&machine.mmio_transports[0], 0x4000, 0x5000, 0x6000);

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
        "expected one block record per disk"
    );
    assert!(
        snap.devices.block_queues[0].is_some(),
        "the activated disk should have captured its queue"
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

// ---------------------------------------------------------------------------
// Rebuild replays the plan's layout and each disk's own activation
// ---------------------------------------------------------------------------

/// Restore must attach every disk where the plan placed it and reactivate
/// exactly the disks that had a session, even when the plan reorders the
/// spec's disks and an inactive disk precedes an active one.
///
/// The spec lists data disk A first and root disk B second; the plan puts the
/// root disk first, so the plan order is [B, A]. Only A is activated. After
/// capture, teardown and rebuild, a read kicked through the transport at A's
/// original address must be served from A's file, and B must stay inactive.
#[tokio::test(flavor = "local")]
async fn rebuild_keeps_plan_layout_and_per_disk_activation_on_kvm() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    use api::block::BlockDevConfig;
    use api::boot::KernelConfig;
    use api::machine::ConsoleConfig;
    use vmm::machine::plan::MachinePlan;

    let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
    let kernel = elf::TempFile::new(&elf::image(&inc_eax_payload(), true));
    // Every sector of both disks differs, so neither the wrong disk nor the
    // wrong offset can produce the expected bytes.
    let disk_a = TempDisk::new(
        "snap-order-a",
        &[[0xa0; SECTOR_SIZE], [0xa1; SECTOR_SIZE]].concat(),
    );
    let disk_b = TempDisk::new(
        "snap-order-b",
        &[[0xb0; SECTOR_SIZE], [0xb1; SECTOR_SIZE]].concat(),
    );
    let disk = |id: &str, path: &Path, root| BlockDevConfig {
        id: Some(id.into()),
        path: path.into(),
        read_only: false,
        root,
    };
    let spec = MachineSpec {
        cpus: 1,
        memory_mib: 16,
        console: ConsoleConfig::None,
        kernel: Some(KernelConfig {
            path: kernel.path().into(),
            cmdline: None,
            initrd: None,
        }),
        disks: vec![
            disk("disk-a", disk_a.path(), false),
            disk("disk-b", disk_b.path(), true),
        ],
    };
    let plan = MachinePlan::new(spec.clone(), context.capabilities()).unwrap();
    // Plan indices are also the disks' indices in the machine's device vectors.
    assert_eq!(
        plan.spec
            .disks
            .iter()
            .map(|disk| disk.id.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["disk-b", "disk-a"]
    );
    let (b, a) = (0, 1);
    let mut machine = build::build(plan, &context).unwrap();
    negotiate_driver_ok(
        &machine.mmio_transports[a],
        DESC_TABLE,
        AVAIL_RING,
        USED_RING,
    );

    machine.pause().await.unwrap();
    let snap = snapshot::capture(&machine).unwrap();
    machine.teardown().await.unwrap();

    // Play A's guest driver in the restored VM's RAM.
    let mut guest = GuestDriver::with_memory(Arc::clone(&snap.memory));
    let mut restored = rebuild(snap, &spec, &context).unwrap();
    // Let the reactivated device finish its first drain, so that the kick
    // below is what delivers the request.
    settle().await;

    let header = guest.request_header(VIRTIO_BLK_T_IN, 1);
    let data = guest.alloc(SECTOR_SIZE as u32);
    let status = guest.alloc(1);
    guest.write_bytes(status, &[0xff]);
    let head = guest.submit(&[
        Segment::readable(header, 16),
        Segment::writable(data, SECTOR_SIZE as u32),
        Segment::writable(status, 1),
    ]);
    // QueueNotify on the transport at A's original address.
    restored.mmio_transports[a].write(0x50, &0u32.to_le_bytes());

    wait_until("disk A to complete a read kicked after rebuild", || {
        guest.used_idx() == 1
    })
    .await;
    assert_eq!(
        guest.used_entry(0),
        (u32::from(head), SECTOR_SIZE as u32 + 1)
    );
    assert_eq!(guest.read_bytes(status, 1), [VIRTIO_BLK_S_OK]);
    assert_eq!(
        guest.read_bytes(data, SECTOR_SIZE),
        [0xa1; SECTOR_SIZE],
        "the read was not served from disk A"
    );

    // B was never activated, so the restore must not have activated it either.
    let mut device_status = [0; 4];
    restored.mmio_transports[b].read(0x70, &mut device_status);
    assert_eq!(u32::from_le_bytes(device_status), 0);
    restored.pause().await.unwrap();
    let snap = snapshot::capture(&restored).unwrap();
    assert_eq!(snap.devices.block_queues[b], None, "disk B was activated");
    assert_eq!(
        snap.devices.block_queues[a].map(|queue| queue.next_used),
        Some(1),
        "disk A did not keep its session"
    );
    restored.teardown().await.unwrap();
}

/// A guest can make its block device task fail. Pausing must survive that,
/// but a machine with a dead device is not snapshotted.
#[tokio::test(flavor = "local")]
async fn pause_survives_an_exited_disk_but_capture_refuses_it_on_kvm() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    use api::block::BlockDevConfig;
    use api::boot::KernelConfig;
    use api::machine::ConsoleConfig;
    use vm_memory::{Bytes, GuestAddress};
    use vmm::machine::plan::MachinePlan;

    let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
    let kernel = elf::TempFile::new(&elf::image(&inc_eax_payload(), true));
    let disk = TempDisk::new("snap-exited", &[0; SECTOR_SIZE]);
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
            id: Some("failing-disk".into()),
            path: disk.path().into(),
            read_only: false,
            root: false,
        }],
    };
    let plan = MachinePlan::new(spec, context.capabilities()).unwrap();
    let mut machine = build::build(plan, &context).unwrap();
    negotiate_driver_ok(
        &machine.mmio_transports[0],
        DESC_TABLE,
        AVAIL_RING,
        USED_RING,
    );

    // Reach the guest RAM through a snapshot taken while the device is healthy.
    machine.pause().await.unwrap();
    let memory = snapshot::capture(&machine).unwrap().memory;
    machine.resume().await.unwrap();

    // An available index past the queue size makes the device task fail.
    memory
        .write_obj(17u16, GuestAddress(AVAIL_RING + 2))
        .unwrap();
    machine.mmio_transports[0].write(0x50, &0u32.to_le_bytes());
    let exit = time::timeout(Duration::from_secs(5), machine.devices.next_exit())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(exit.name, "failing-disk");
    assert!(exit.result.is_err());

    machine.pause().await.unwrap();
    let Err(error) = snapshot::capture(&machine) else {
        panic!("captured a machine whose block device has exited");
    };
    assert!(error.to_string().contains("has exited"), "{error:#}");
    machine.resume().await.unwrap();
    machine.teardown().await.unwrap();
}
