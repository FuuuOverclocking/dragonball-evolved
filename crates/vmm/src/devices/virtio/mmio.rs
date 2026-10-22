use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use logger::warn;
use virtio_queue::{Queue, QueueT};
use vmm_sys_util::eventfd::EventFd;

use super::{
    Activation, Activator, InterruptKind, IrqTrigger, SessionControl, SharedGuestMemory,
    session_channel,
};
use crate::machine::bus::BusDevice;

pub const VERSION_1: u64 = 1 << 32;
pub const NOTIFY_OFFSET: u64 = 0x50;
const ACKNOWLEDGE: u32 = 1;
const DRIVER: u32 = 2;
const NEEDS_RESET: u32 = 64;
const FAILED: u32 = 128;

#[derive(Debug, Clone)]
pub struct DeviceDesc {
    pub device_type: u32,
    pub features: u64,
    pub queue_max: Vec<u16>,
    pub config: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueRegs {
    pub size: u32,
    pub ready: bool,
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MmioRegs {
    pub status: u32,
    pub features_sel: u32,
    pub driver_features_sel: u32,
    pub driver_features: u64,
    pub queue_sel: u32,
    pub queues: Vec<QueueRegs>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    Activate,
    Reset,
    AckInterrupt(u32),
    Notify(u16),
}

impl MmioRegs {
    pub fn new(desc: &DeviceDesc) -> Self {
        Self {
            queues: vec![QueueRegs::default(); desc.queue_max.len()],
            ..Default::default()
        }
    }
}

pub fn read(regs: &MmioRegs, desc: &DeviceDesc, irq_status: u32, offset: u64) -> u32 {
    match offset {
        0x00 => 0x7472_6976,
        0x04 => 2,
        0x08 => desc.device_type,
        0x0c => 0,
        0x10 if regs.features_sel < 2 => {
            ((desc.features | VERSION_1) >> (regs.features_sel * 32)) as u32
        }
        0x34 => desc
            .queue_max
            .get(regs.queue_sel as usize)
            .copied()
            .map(u32::from)
            .unwrap_or(0),
        0x44 => regs
            .queues
            .get(regs.queue_sel as usize)
            .map_or(0, |queue| u32::from(queue.ready)),
        0x60 => irq_status,
        0x70 => regs.status,
        _ => 0,
    }
}

pub fn write(
    mut regs: MmioRegs,
    desc: &DeviceDesc,
    offset: u64,
    value: u32,
) -> (MmioRegs, Option<Transition>) {
    let mut transition = None;
    match offset {
        0x14 => regs.features_sel = value,
        0x24 => regs.driver_features_sel = value,
        0x30 => regs.queue_sel = value,
        0x20 if regs.status == ACKNOWLEDGE | DRIVER && regs.driver_features_sel < 2 => {
            let shift = regs.driver_features_sel * 32;
            regs.driver_features = (regs.driver_features & !(u64::from(u32::MAX) << shift))
                | (u64::from(value) << shift);
        }
        0x70 if value == 0 => transition = Some(Transition::Reset),
        0x70 if value & FAILED != 0 => regs.status |= FAILED,
        0x70 => {
            let valid = matches!((regs.status, value), (0, 1) | (1, 3) | (3, 11) | (11, 15));
            let features_valid = regs.driver_features & !(desc.features | VERSION_1) == 0
                && regs.driver_features & VERSION_1 != 0;
            if valid && (value != 11 || features_valid) {
                regs.status = value;
                if value == 15 {
                    transition = Some(Transition::Activate);
                }
            }
        }
        0x64 => transition = Some(Transition::AckInterrupt(value)),
        NOTIFY_OFFSET if regs.status == 15 && (value as usize) < desc.queue_max.len() => {
            transition = Some(Transition::Notify(value as u16));
        }
        0x38 | 0x44 | 0x80 | 0x84 | 0x90 | 0x94 | 0xa0 | 0xa4 if regs.status == 11 => {
            if let Some(queue) = regs.queues.get_mut(regs.queue_sel as usize) {
                if offset == 0x44 {
                    if value <= 1 {
                        queue.ready = value == 1;
                    }
                } else if !queue.ready {
                    match offset {
                        0x38 => queue.size = value,
                        0x80 => queue.desc = (queue.desc & !u64::from(u32::MAX)) | u64::from(value),
                        0x84 => {
                            queue.desc =
                                (queue.desc & u64::from(u32::MAX)) | (u64::from(value) << 32)
                        }
                        0x90 => {
                            queue.avail = (queue.avail & !u64::from(u32::MAX)) | u64::from(value)
                        }
                        0x94 => {
                            queue.avail =
                                (queue.avail & u64::from(u32::MAX)) | (u64::from(value) << 32)
                        }
                        0xa0 => queue.used = (queue.used & !u64::from(u32::MAX)) | u64::from(value),
                        0xa4 => {
                            queue.used =
                                (queue.used & u64::from(u32::MAX)) | (u64::from(value) << 32)
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }
    (regs, transition)
}

struct TransportState {
    regs: MmioRegs,
    session: Option<SessionControl>,
}

pub struct MmioTransport {
    state: Mutex<TransportState>,
    desc: DeviceDesc,
    memory: SharedGuestMemory,
    activator: Activator,
    interrupt: Arc<IrqTrigger>,
    queue_notifiers: Vec<EventFd>,
}

impl MmioTransport {
    pub fn new(
        desc: DeviceDesc,
        memory: SharedGuestMemory,
        activator: Activator,
        interrupt: Arc<IrqTrigger>,
        queue_notifiers: Vec<EventFd>,
    ) -> Self {
        let regs = MmioRegs::new(&desc);
        Self {
            state: Mutex::new(TransportState {
                regs,
                session: None,
            }),
            desc,
            memory,
            activator,
            interrupt,
            queue_notifiers,
        }
    }

    /// Construct a transport with previously captured register and interrupt
    /// state, used during snapshot restore.
    pub fn with_state(
        desc: DeviceDesc,
        memory: SharedGuestMemory,
        activator: Activator,
        interrupt: Arc<IrqTrigger>,
        queue_notifiers: Vec<EventFd>,
        regs: MmioRegs,
        interrupt_status: u32,
    ) -> Self {
        interrupt.status().store(interrupt_status, Ordering::SeqCst);
        Self {
            state: Mutex::new(TransportState {
                regs,
                session: None,
            }),
            desc,
            memory,
            activator,
            interrupt,
            queue_notifiers,
        }
    }

    fn activate(&self, state: &mut TransportState) -> Result<()> {
        let mut queues = Vec::with_capacity(self.desc.queue_max.len());
        for (regs, &max) in state.regs.queues.iter().zip(&self.desc.queue_max) {
            if regs.size == 0
                || regs.size > u32::from(max)
                || !regs.size.is_power_of_two()
                || !regs.ready
            {
                bail!("invalid virtio queue size or readiness");
            }
            if !regs.desc.is_multiple_of(16)
                || !regs.avail.is_multiple_of(2)
                || !regs.used.is_multiple_of(4)
            {
                bail!("virtio queue addresses are misaligned");
            }
            let mut queue = Queue::new(max)?;
            queue.set_size(regs.size as u16);
            queue.set_ready(true);
            queue.set_desc_table_address(Some(regs.desc as u32), Some((regs.desc >> 32) as u32));
            queue.set_avail_ring_address(Some(regs.avail as u32), Some((regs.avail >> 32) as u32));
            queue.set_used_ring_address(Some(regs.used as u32), Some((regs.used >> 32) as u32));
            if !queue.is_valid(self.memory.as_ref()) {
                bail!("invalid virtio queue address");
            }
            queues.push(queue);
        }
        let (control, session) = session_channel();
        self.activator.activate(Activation {
            mem: Arc::clone(&self.memory),
            queues,
            interrupt: Arc::clone(&self.interrupt),
            driver_features: state.regs.driver_features,
            session,
        })?;
        state.session = Some(control);
        for notifier in &self.queue_notifiers {
            notifier.write(1).context("kick activated queue")?;
        }
        Ok(())
    }

    fn reset(&self, state: &mut TransportState) -> Result<()> {
        if let Some(session) = &state.session {
            session.reset()?;
        }
        state.session = None;
        self.interrupt.status().store(0, Ordering::SeqCst);
        state.regs = MmioRegs::new(&self.desc);
        Ok(())
    }

    /// Return a clone of the transport's register state and the current
    /// interrupt status word. Intended for snapshot capture.
    pub fn snapshot(&self) -> (MmioRegs, u32) {
        let state = self.state.lock().unwrap();
        (
            state.regs.clone(),
            self.interrupt.status().load(Ordering::SeqCst),
        )
    }
}

impl BusDevice for MmioTransport {
    fn read(&self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        if offset >= 0x100 {
            let start = (offset - 0x100) as usize;
            if let Some(end) = start.checked_add(data.len())
                && let Some(config) = self.desc.config.get(start..end)
            {
                data.copy_from_slice(config);
            }
        } else if data.len() == 4 && offset.is_multiple_of(4) {
            let state = self.state.lock().unwrap();
            let value = read(
                &state.regs,
                &self.desc,
                self.interrupt.status().load(Ordering::SeqCst),
                offset,
            );
            data.copy_from_slice(&value.to_le_bytes());
        }
    }

    fn write(&self, offset: u64, data: &[u8]) {
        if data.len() != 4 || !offset.is_multiple_of(4) || offset >= 0x100 {
            return;
        }
        let value = u32::from_le_bytes(data.try_into().unwrap());
        let mut state = self.state.lock().unwrap();
        let (next, transition) = write(std::mem::take(&mut state.regs), &self.desc, offset, value);
        state.regs = next;
        let result = match transition {
            Some(Transition::Activate) => self.activate(&mut state),
            Some(Transition::Reset) => self.reset(&mut state),
            Some(Transition::AckInterrupt(bits)) => {
                self.interrupt.status().fetch_and(!bits, Ordering::SeqCst);
                Ok(())
            }
            Some(Transition::Notify(queue)) => self.queue_notifiers[queue as usize]
                .write(1)
                .map_err(Into::into),
            None => Ok(()),
        };
        if let Err(error) = result {
            warn!("virtio-mmio transition failed: {error:#}");
            state.regs.status |= NEEDS_RESET;
            let _ = self.interrupt.trigger(InterruptKind::Config);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc() -> DeviceDesc {
        DeviceDesc {
            device_type: 2,
            features: VERSION_1 | (1 << 9),
            queue_max: vec![256],
            config: vec![0; 8],
        }
    }

    #[test]
    fn negotiation_and_invalid_transitions() {
        let desc = desc();
        let (regs, effect) = write(MmioRegs::new(&desc), &desc, 0x70, 15);
        assert_eq!(regs.status, 0);
        assert_eq!(effect, None);
        let (regs, _) = write(regs, &desc, 0x70, 1);
        let (regs, _) = write(regs, &desc, 0x70, 3);
        let (regs, _) = write(regs, &desc, 0x70, 11);
        assert_eq!(regs.status, 3);
        let (regs, _) = write(regs, &desc, 0x24, 1);
        let (regs, _) = write(regs, &desc, 0x20, 1);
        let (regs, _) = write(regs, &desc, 0x70, 11);
        assert_eq!(regs.status, 11);
        let (regs, effect) = write(regs, &desc, 0x70, 15);
        assert_eq!(regs.status, 15);
        assert_eq!(effect, Some(Transition::Activate));
        let (regs, effect) = write(regs, &desc, 0x70, 0);
        assert_eq!(effect, Some(Transition::Reset));
        assert_eq!(regs.status, 15);
    }

    #[test]
    fn large_feature_select_and_queue_index_are_safe() {
        let desc = desc();
        let (regs, _) = write(MmioRegs::new(&desc), &desc, 0x14, u32::MAX);
        assert_eq!(read(&regs, &desc, 0, 0x10), 0);
        let (regs, _) = write(regs, &desc, 0x30, u32::MAX);
        assert_eq!(read(&regs, &desc, 0, 0x34), 0);
    }
}
