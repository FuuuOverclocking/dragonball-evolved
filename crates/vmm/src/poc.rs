use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use api::{PocConfig, PocReport};
use tokio::sync::mpsc;
use virtio_queue::{Queue, QueueT};
use vm_memory::{Address, Bytes, GuestAddress};

use crate::devices::DeviceManager;
use crate::devices::virtio::Activation;
use crate::devices::virtio::block::BlockDevice;
use crate::memory::{GuestRam, SharedMemory};
use crate::vcpu::{Vcpu, VcpuExit, VcpuHandle};
use crate::vm::Vm;

const MEMORY_SIZE: u64 = 0x40_000;
const ENTRY: u64 = 0x8000;
const QUEUE_SIZE: u16 = 16;
const DESC_TABLE: u64 = 0x1000;
const AVAIL_RING: u64 = 0x2000;
const USED_RING: u64 = 0x3000;
const REQUEST_HEADER: u64 = 0x10_000;
const DATA: u64 = 0x11_000;
const STATUS: u64 = 0x12_000;
const SECTOR_SIZE: u32 = 512;
const KICK_PORT: u64 = 0x1000;
const EXIT_PORT: u16 = 0x1001;
const BLOCK_GSI: u32 = 5;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

// 16-bit real-mode code:
//   out 0x1000, al              kernel signals the block ioeventfd
//   while (*(u16 *)0x3002 != 1) guest polls the used ring
//   out 0x1001, al              userspace completion exit
const GUEST_CODE: &[u8] = &[
    0xba, 0x00, 0x10, // mov dx, 0x1000
    0x30, 0xc0, // xor al, al
    0xee, // out dx, al
    0xbb, 0x02, 0x30, // mov bx, 0x3002
    0x83, 0x3f, 0x01, // cmp word [bx], 1
    0x75, 0xfb, // jne -5
    0xba, 0x01, 0x10, // mov dx, 0x1001
    0xee, // out dx, al
];

/// Resources that must outlive the KVM vCPU and its async block task.
pub struct PocMachine {
    ram: GuestRam,
    _vm: Vm,
    vcpu: VcpuHandle,
    interrupt_status: Arc<std::sync::atomic::AtomicU32>,
}

impl PocMachine {
    pub fn start(
        config: &PocConfig,
        devices: &mut DeviceManager,
        exits: mpsc::UnboundedSender<VcpuExit>,
    ) -> Result<Self> {
        let ram = GuestRam::new(MEMORY_SIZE)?;
        let memory = ram.memory();
        let queue = prepare_guest(&memory)?;
        let vm = Vm::new(&ram)?;

        let (block, handle) = BlockDevice::new("poc-block", &config.disk_path, true, None)?;
        vm.register_pio_event(&handle.queue_notifier, KICK_PORT)?;
        vm.register_irq(handle.interrupt.irq_fd(), BLOCK_GSI)?;
        let interrupt_status = handle.interrupt.status();

        let (vcpu, control) = Vcpu::new(&vm, ENTRY, EXIT_PORT)?;
        handle.activator.activate(Activation {
            mem: Arc::clone(&memory),
            queues: vec![queue],
            interrupt: Arc::clone(&handle.interrupt),
        })?;
        let vcpu = vcpu.start(control, exits)?;

        let stop = devices.stop_signal();
        devices.spawn("poc-block", block.run(stop));

        Ok(Self {
            ram,
            _vm: vm,
            vcpu,
            interrupt_status,
        })
    }

    pub fn memory_bytes(&self) -> u64 {
        self.ram.size()
    }

    pub fn finish(mut self) -> Result<PocReport> {
        self.vcpu.stop()?;
        let memory = self.ram.memory();
        Ok(PocReport {
            used_index: memory
                .read_obj(GuestAddress(USED_RING + 2))
                .context("read used index")?,
            request_status: memory
                .read_obj(GuestAddress(STATUS))
                .context("read block status")?,
            data: {
                let mut data = vec![0; 16];
                memory
                    .read_slice(&mut data, GuestAddress(DATA))
                    .context("read block response")?;
                data
            },
            interrupt_status: self.interrupt_status.load(Ordering::SeqCst),
        })
    }

    pub fn stop(mut self) -> Result<()> {
        self.vcpu.stop()
    }
}

fn prepare_guest(memory: &SharedMemory) -> Result<Queue> {
    memory
        .write_slice(GUEST_CODE, GuestAddress(ENTRY))
        .context("write guest code")?;

    // VIRTIO_BLK_T_IN, sector 0.
    memory
        .write_obj(0_u32, GuestAddress(REQUEST_HEADER))
        .context("write request type")?;
    memory.write_obj(0_u32, GuestAddress(REQUEST_HEADER + 4))?;
    memory.write_obj(0_u64, GuestAddress(REQUEST_HEADER + 8))?;

    write_descriptor(
        memory,
        0,
        GuestAddress(REQUEST_HEADER),
        16,
        VIRTQ_DESC_F_NEXT,
        1,
    )?;
    write_descriptor(
        memory,
        1,
        GuestAddress(DATA),
        SECTOR_SIZE,
        VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
        2,
    )?;
    write_descriptor(memory, 2, GuestAddress(STATUS), 1, VIRTQ_DESC_F_WRITE, 0)?;

    memory.write_obj(0_u16, GuestAddress(AVAIL_RING))?;
    memory.write_obj(1_u16, GuestAddress(AVAIL_RING + 2))?;
    memory.write_obj(0_u16, GuestAddress(AVAIL_RING + 4))?;
    memory.write_obj(0_u16, GuestAddress(USED_RING))?;
    memory.write_obj(0_u16, GuestAddress(USED_RING + 2))?;

    let mut queue = Queue::new(QUEUE_SIZE).context("create POC virtqueue")?;
    queue.set_size(QUEUE_SIZE);
    queue.set_desc_table_address(Some(DESC_TABLE as u32), Some(0));
    queue.set_avail_ring_address(Some(AVAIL_RING as u32), Some(0));
    queue.set_used_ring_address(Some(USED_RING as u32), Some(0));
    queue.set_ready(true);
    Ok(queue)
}

fn write_descriptor(
    memory: &SharedMemory,
    index: u16,
    address: GuestAddress,
    len: u32,
    flags: u16,
    next: u16,
) -> Result<()> {
    let at = GuestAddress(DESC_TABLE + u64::from(index) * 16);
    memory.write_obj(address.raw_value(), at)?;
    memory.write_obj(len, at.unchecked_add(8))?;
    memory.write_obj(flags, at.unchecked_add(12))?;
    memory.write_obj(next, at.unchecked_add(14))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_layout_contains_one_read_request() {
        let ram = GuestRam::new(MEMORY_SIZE).unwrap();
        let memory = ram.memory();
        let queue = prepare_guest(&memory).unwrap();

        assert!(queue.is_valid(memory.as_ref()));
        assert_eq!(
            memory
                .read_obj::<u16>(GuestAddress(AVAIL_RING + 2))
                .unwrap(),
            1
        );
        assert_eq!(
            memory
                .read_obj::<u32>(GuestAddress(REQUEST_HEADER))
                .unwrap(),
            0
        );
        let mut code = vec![0; GUEST_CODE.len()];
        memory.read_slice(&mut code, GuestAddress(ENTRY)).unwrap();
        assert_eq!(code, GUEST_CODE);
    }
}
