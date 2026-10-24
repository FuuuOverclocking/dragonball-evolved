//! Snapshot round-trip integration tests: a paused machine is captured, torn
//! down and rebuilt from the snapshot, and the guest carries on.

mod common;
#[allow(dead_code)]
#[path = "common/elf.rs"]
mod elf;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use api::boot::KernelConfig;
use api::machine::ConsoleConfig;
use common::guest::{
    AVAIL_RING, DESC_TABLE, GuestDriver, SECTOR_SIZE, Segment, USED_RING, VIRTIO_BLK_S_OK,
    VIRTIO_BLK_T_IN,
};
use common::{TempDisk, settle, wait_until};
use tokio::time;
use vmm::KvmContext;
use vmm::machine::build::{self, Machine};
use vmm::machine::bus::Bus;
use vmm::machine::plan::{DevicePlan, MachinePlan, VirtioMmioGrant};
use vmm::machine::snapshot::{self, rebuild};
use vmm::resources::{GpaRange, Gsi};
use vmm::spec::{BlockSpec, DeviceSpec, MachineSpec};

const QUEUE_NOTIFY: u64 = 0x50;
const STATUS: u64 = 0x70;

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

fn spec(kernel: &Path, devices: Vec<DeviceSpec>) -> MachineSpec {
    MachineSpec {
        cpus: 1,
        memory_mib: 16,
        console: ConsoleConfig::None,
        kernel: Some(KernelConfig {
            path: kernel.into(),
            cmdline: None,
            initrd: None,
        }),
        devices,
    }
}

fn disk(id: &str, path: &Path, root: bool) -> DeviceSpec {
    DeviceSpec::Block(BlockSpec {
        id: id.into(),
        path: path.into(),
        read_only: false,
        root,
    })
}

fn build_machine(spec: MachineSpec, context: &KvmContext) -> Machine {
    let plan = MachinePlan::new(spec, context.capabilities()).unwrap();
    build::build(plan, context).unwrap()
}

/// Where the plan put the transport of disk `id`.
fn window(machine: &Machine, id: &str) -> u64 {
    machine
        .plan()
        .devices
        .iter()
        .find_map(|device| match device {
            DevicePlan::Block(block) if block.id == id => Some(block.grant.mmio.start()),
            _ => None,
        })
        .unwrap()
}

/// Negotiate the virtio-mmio transport at `base` up to DRIVER_OK the way a guest
/// driver would, through the bus the vCPUs use: status 0→1→3, set features,
/// status 3→11, set the queue addresses, set queue ready, status 11→15.
fn negotiate_driver_ok(bus: &Bus, base: u64) {
    for (offset, value) in [
        // status = ACKNOWLEDGE, then ACKNOWLEDGE | DRIVER
        (STATUS, 1u32),
        (STATUS, 3),
        // driver_features_sel = 1 (high word), then VERSION_1 (its bit 0)
        (0x24, 1),
        (0x20, 1),
        // status = ACKNOWLEDGE | DRIVER | FEATURES_OK
        (STATUS, 11),
        // queue size, then the low words of desc table, avail ring, used ring
        (0x38, 16),
        (0x80, DESC_TABLE as u32),
        (0x90, AVAIL_RING as u32),
        (0xa0, USED_RING as u32),
        // queue ready, then status = DRIVER_OK
        (0x44, 1),
        (STATUS, 15),
    ] {
        assert!(bus.write(base + offset, &value.to_le_bytes()));
    }
}

/// Submit a one-sector read of `sector`, kick the transport at `base`, and
/// return what the device wrote into the guest's buffer.
async fn read_sector(guest: &mut GuestDriver, bus: &Bus, base: u64, sector: u64) -> Vec<u8> {
    let used = guest.used_idx();
    let header = guest.request_header(VIRTIO_BLK_T_IN, sector);
    let data = guest.alloc(SECTOR_SIZE as u32);
    let status = guest.alloc(1);
    guest.write_bytes(status, &[0xff]);
    let head = guest.submit(&[
        Segment::readable(header, 16),
        Segment::writable(data, SECTOR_SIZE as u32),
        Segment::writable(status, 1),
    ]);
    assert!(bus.write(base + QUEUE_NOTIFY, &0u32.to_le_bytes()));
    wait_until("a read kicked after rebuild to complete", || {
        guest.used_idx() == used.wrapping_add(1)
    })
    .await;
    assert_eq!(
        guest.used_entry(used),
        (u32::from(head), SECTOR_SIZE as u32 + 1)
    );
    assert_eq!(guest.read_bytes(status, 1), [VIRTIO_BLK_S_OK]);
    guest.read_bytes(data, SECTOR_SIZE)
}

/// A register-incrementing guest keeps running across capture, teardown,
/// rebuild and resume: rax is past its captured value afterwards.
#[tokio::test(flavor = "local")]
async fn register_increment_round_trip_on_kvm() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
    let kernel = elf::TempFile::new(&elf::image(&inc_eax_payload(), true));
    let mut machine = build_machine(spec(kernel.path(), Vec::new()), &context);

    machine.vcpus.as_ref().unwrap().resume().await.unwrap();
    time::sleep(Duration::from_millis(50)).await;
    machine.pause().await.unwrap();
    let snap = snapshot::capture(&machine).unwrap();
    let rax_before = snap.vcpus[0].regs.rax;
    assert!(rax_before > 0, "guest did not execute (rax = {rax_before})");

    // The snapshot's memory Arc keeps the guest RAM mapped across the teardown.
    machine.teardown().await.unwrap();
    let mut restored = rebuild(snap, &context).unwrap();

    restored.vcpus.as_ref().unwrap().resume().await.unwrap();
    time::sleep(Duration::from_millis(50)).await;
    restored.pause().await.unwrap();
    let rax_after = snapshot::capture(&restored).unwrap().vcpus[0].regs.rax;
    assert!(
        rax_after > rax_before,
        "guest did not advance after rebuild (rax: {rax_before} -> {rax_after})"
    );
    restored.teardown().await.unwrap();
}

/// Restore must attach every disk where the plan placed it and reactivate
/// exactly the disks that had a session, even when the plan reorders the
/// spec's disks and an inactive disk precedes an active one.
///
/// The spec lists data disk A first and root disk B second; the plan puts the
/// root disk first. Only A is activated. After capture, teardown and rebuild, a
/// read kicked at A's address must be served from A's file, B must stay
/// inactive, and the restored machine must carry the captured plan.
#[tokio::test(flavor = "local")]
async fn rebuild_keeps_plan_layout_and_per_disk_activation_on_kvm() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
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
    let mut machine = build_machine(
        spec(
            kernel.path(),
            vec![
                disk("disk-a", disk_a.path(), false),
                disk("disk-b", disk_b.path(), true),
            ],
        ),
        &context,
    );
    assert_eq!(
        machine
            .plan()
            .devices
            .iter()
            .map(DevicePlan::id)
            .collect::<Vec<_>>(),
        ["disk-b", "disk-a"]
    );
    let (a, b) = (window(&machine, "disk-a"), window(&machine, "disk-b"));
    negotiate_driver_ok(machine.mmio_bus(), a);

    machine.pause().await.unwrap();
    let snap = snapshot::capture(&machine).unwrap();
    machine.teardown().await.unwrap();
    assert_eq!(snap.block("disk-a").unwrap().transport.regs.status, 15);
    assert!(snap.block("disk-a").unwrap().queue.is_some());
    assert_eq!(snap.block("disk-b").unwrap().queue, None);

    // Play A's guest driver in the restored VM's RAM.
    let mut guest = GuestDriver::with_memory(Arc::clone(&snap.memory));
    let captured_plan = snap.plan.clone();
    let mut restored = rebuild(snap, &context).unwrap();
    assert_eq!(restored.plan(), &captured_plan);
    // Let the reactivated device finish its first drain, so that the kick
    // below is what delivers the request.
    settle().await;

    assert_eq!(
        read_sector(&mut guest, restored.mmio_bus(), a, 1).await,
        [0xa1; SECTOR_SIZE],
        "the read was not served from disk A"
    );

    // B was never activated, so the restore must not have activated it either.
    let mut device_status = [0; 4];
    assert!(restored.mmio_bus().read(b + STATUS, &mut device_status));
    assert_eq!(u32::from_le_bytes(device_status), 0);
    restored.pause().await.unwrap();
    let snap = snapshot::capture(&restored).unwrap();
    assert_eq!(
        snap.block("disk-b").unwrap().queue,
        None,
        "disk B was activated"
    );
    assert_eq!(
        snap.block("disk-a")
            .unwrap()
            .queue
            .map(|queue| queue.next_used),
        Some(1),
        "disk A did not keep its session"
    );
    restored.teardown().await.unwrap();
}

/// A restore attaches each device at the grant the snapshot recorded, not at
/// one recomputed from the spec: moving a disk's grant in the snapshot moves
/// the disk, with its session, in the restored machine.
#[tokio::test(flavor = "local")]
async fn rebuild_attaches_devices_at_the_recorded_grants_on_kvm() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
    let kernel = elf::TempFile::new(&elf::image(&inc_eax_payload(), true));
    let disk_file = TempDisk::new(
        "snap-grant",
        &[[0xc0; SECTOR_SIZE], [0xc1; SECTOR_SIZE]].concat(),
    );
    let mut machine = build_machine(
        spec(kernel.path(), vec![disk("disk", disk_file.path(), false)]),
        &context,
    );
    let planned = window(&machine, "disk");
    negotiate_driver_ok(machine.mmio_bus(), planned);
    machine.pause().await.unwrap();
    let mut snap = snapshot::capture(&machine).unwrap();
    machine.teardown().await.unwrap();

    let moved = VirtioMmioGrant {
        mmio: GpaRange::new(0xc000_8000, 0xc000_9000),
        gsi: Gsi(12),
    };
    let DevicePlan::Block(block) = &mut snap.plan.devices[0];
    assert_ne!(block.grant, moved);
    block.grant = moved;

    let mut guest = GuestDriver::with_memory(Arc::clone(&snap.memory));
    let mut restored = rebuild(snap, &context).unwrap();
    settle().await;
    let mut device_status = [0; 4];
    assert!(
        !restored
            .mmio_bus()
            .read(planned + STATUS, &mut device_status),
        "something is still attached at the planned address"
    );
    assert_eq!(
        read_sector(&mut guest, restored.mmio_bus(), moved.mmio.start(), 1).await,
        [0xc1; SECTOR_SIZE]
    );
    restored.pause().await.unwrap();
    restored.teardown().await.unwrap();
}

/// A guest can make its block device task fail. Pausing must survive that,
/// but a machine with a dead device is not snapshotted.
#[tokio::test(flavor = "local")]
async fn pause_survives_an_exited_disk_but_capture_refuses_it_on_kvm() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    use vm_memory::{Bytes, GuestAddress};

    let context = KvmContext::from_dev_path("/dev/kvm").unwrap();
    let kernel = elf::TempFile::new(&elf::image(&inc_eax_payload(), true));
    let disk_file = TempDisk::new("snap-exited", &[0; SECTOR_SIZE]);
    let mut machine = build_machine(
        spec(
            kernel.path(),
            vec![disk("failing-disk", disk_file.path(), false)],
        ),
        &context,
    );
    let base = window(&machine, "failing-disk");
    negotiate_driver_ok(machine.mmio_bus(), base);

    // Reach the guest RAM through a snapshot taken while the device is healthy.
    machine.pause().await.unwrap();
    let memory = snapshot::capture(&machine).unwrap().memory;
    machine.resume().await.unwrap();

    // An available index past the queue size makes the device task fail.
    memory
        .write_obj(17u16, GuestAddress(AVAIL_RING + 2))
        .unwrap();
    assert!(
        machine
            .mmio_bus()
            .write(base + QUEUE_NOTIFY, &0u32.to_le_bytes())
    );
    let exit = time::timeout(Duration::from_secs(5), machine.tasks.next_exit())
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
