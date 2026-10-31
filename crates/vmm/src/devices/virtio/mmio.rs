use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{io, mem};

use logger::warn;
use virtio_queue::{Queue, QueueT};
use vmm_sys_util::eventfd::EventFd;

use super::{
    Activation, ActivationError, Activator, InterruptKind, IrqTrigger, ResetError, SessionControl,
    session_channel,
};
use crate::bus::BusDevice;
use crate::memory::store::MemoryView;
use crate::space::Gpa;

pub const VERSION_1: u64 = 1 << 32;
pub const NOTIFY_OFFSET: u64 = 0x50;
const ACKNOWLEDGE: u32 = 1;
const DRIVER: u32 = 2;
pub const NEEDS_RESET: u32 = 64;
const FAILED: u32 = 128;
const RESET_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDesc {
    pub device_type: u32,
    pub features: u64,
    pub queue_max: Vec<u16>,
    pub config: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRegs {
    pub size: u32,
    pub ready: bool,
    pub desc: Gpa,
    pub avail: Gpa,
    pub used: Gpa,
}

impl Default for QueueRegs {
    fn default() -> Self {
        Self {
            size: 0,
            ready: false,
            desc: Gpa(0),
            avail: Gpa(0),
            used: Gpa(0),
        }
    }
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum QueueError {
    /// invalid virtio queue size or readiness
    Geometry,
    /// virtio queue addresses are misaligned
    Alignment,
    /// create the virtio queue
    Create(#[source] virtio_queue::Error),
    /// virtio queue lies outside guest memory
    Bounds,
}

impl QueueRegs {
    pub fn queue(&self, max: u16, memory: &MemoryView) -> Result<Queue, QueueError> {
        if self.size == 0
            || self.size > u32::from(max)
            || !self.size.is_power_of_two()
            || !self.ready
        {
            return Err(QueueError::Geometry);
        }
        if !self.desc.0.is_multiple_of(16)
            || !self.avail.0.is_multiple_of(2)
            || !self.used.0.is_multiple_of(4)
        {
            return Err(QueueError::Alignment);
        }
        let mut queue = Queue::new(max).map_err(QueueError::Create)?;
        queue.set_size(self.size as u16);
        queue.set_ready(true);
        queue.set_desc_table_address(Some(self.desc.0 as u32), Some((self.desc.0 >> 32) as u32));
        queue.set_avail_ring_address(Some(self.avail.0 as u32), Some((self.avail.0 >> 32) as u32));
        queue.set_used_ring_address(Some(self.used.0 as u32), Some((self.used.0 >> 32) as u32));
        if !queue.is_valid(memory.as_ref()) {
            return Err(QueueError::Bounds);
        }
        Ok(queue)
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmioTransportSnapshot {
    pub desc: DeviceDesc,
    pub regs: MmioRegs,
    pub interrupt_status: u32,
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
        0x10 if regs.features_sel < 2 => {
            ((desc.features | VERSION_1) >> (regs.features_sel * 32)) as u32
        }
        0x34 => desc
            .queue_max
            .get(regs.queue_sel as usize)
            .copied()
            .map_or(0, u32::from),
        0x44 => regs
            .queues
            .get(regs.queue_sel as usize)
            .map_or(0, |queue| u32::from(queue.ready)),
        0x60 => irq_status,
        0x70 => regs.status,
        0xb0 | 0xb4 | 0xb8 | 0xbc => u32::MAX,
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
        NOTIFY_OFFSET
            if regs.status == 15
                && (value as usize) < desc.queue_max.len()
                && value <= u32::from(u16::MAX) =>
        {
            if regs
                .queues
                .get(value as usize)
                .is_some_and(|queue| queue.ready)
            {
                transition = Some(Transition::Notify(value as u16));
            }
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
                        0x80 => queue.desc = low(queue.desc, value),
                        0x84 => queue.desc = high(queue.desc, value),
                        0x90 => queue.avail = low(queue.avail, value),
                        0x94 => queue.avail = high(queue.avail, value),
                        0xa0 => queue.used = low(queue.used, value),
                        0xa4 => queue.used = high(queue.used, value),
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }
    (regs, transition)
}

fn low(address: Gpa, value: u32) -> Gpa {
    Gpa((address.0 & !u64::from(u32::MAX)) | u64::from(value))
}

fn high(address: Gpa, value: u32) -> Gpa {
    Gpa((address.0 & u64::from(u32::MAX)) | (u64::from(value) << 32))
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum TransportError {
    /// an old virtio session still owns the queues
    Active,
    /// configure the virtio queues
    Queue(#[from] QueueError),
    /// send the virtio activation
    Activate(#[from] ActivationError),
    /// reset the virtio session
    Reset(#[from] ResetError),
    /// queue notifier count does not match the device
    Notifiers,
    /// notify the virtio queue
    Notify(#[source] io::Error),
}

struct TransportState {
    regs: MmioRegs,
    session: Option<SessionControl>,
}

pub struct MmioTransport {
    state: Mutex<TransportState>,
    desc: DeviceDesc,
    memory: MemoryView,
    activator: Activator,
    interrupt: Arc<IrqTrigger>,
    queue_notifiers: Vec<EventFd>,
}

impl MmioTransport {
    pub fn new(
        desc: DeviceDesc,
        memory: MemoryView,
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

    fn activate(&self, state: &mut TransportState) -> Result<(), TransportError> {
        if state.session.is_some() {
            return Err(TransportError::Active);
        }
        if self.queue_notifiers.len() != self.desc.queue_max.len()
            || state.regs.queues.len() != self.desc.queue_max.len()
        {
            return Err(TransportError::Notifiers);
        }
        let queues = state
            .regs
            .queues
            .iter()
            .zip(&self.desc.queue_max)
            .map(|(regs, &max)| regs.queue(max, &self.memory))
            .collect::<Result<Vec<_>, _>>()?;
        let (control, session) = session_channel();
        self.activator.activate(Activation {
            queues,
            mem: Arc::clone(&self.memory),
            interrupt: Arc::clone(&self.interrupt),
            driver_features: state.regs.driver_features,
            session,
        })?;
        state.session = Some(control);
        for notifier in &self.queue_notifiers {
            notifier.write(1).map_err(TransportError::Notify)?;
        }
        Ok(())
    }

    fn reset(&self, state: &mut TransportState) -> Result<(), TransportError> {
        if let Some(session) = &mut state.session {
            session.reset(RESET_WAIT)?;
        }
        state.session = None;
        self.interrupt.acknowledge(u32::MAX);
        state.regs = MmioRegs::new(&self.desc);
        Ok(())
    }
}

impl BusDevice<Gpa> for MmioTransport {
    fn read(&self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        if offset >= 0x100 {
            if let Ok(start) = usize::try_from(offset - 0x100)
                && let Some(end) = start.checked_add(data.len())
                && let Some(config) = self.desc.config.get(start..end)
            {
                data.copy_from_slice(config);
            }
        } else if data.len() == 4 && offset.is_multiple_of(4) {
            let state = self.state.lock().unwrap();
            data.copy_from_slice(
                &read(&state.regs, &self.desc, self.interrupt.status(), offset).to_le_bytes(),
            );
        }
    }

    fn write(&self, offset: u64, data: &[u8]) {
        if data.len() != 4 || !offset.is_multiple_of(4) || offset >= 0x100 {
            return;
        }
        let value = u32::from_le_bytes(data.try_into().unwrap());
        let mut state = self.state.lock().unwrap();
        let (next, transition) = write(mem::take(&mut state.regs), &self.desc, offset, value);
        state.regs = next;
        let result = match transition {
            Some(Transition::Activate) => self.activate(&mut state),
            Some(Transition::Reset) => self.reset(&mut state),
            Some(Transition::AckInterrupt(bits)) => {
                self.interrupt.acknowledge(bits);
                Ok(())
            }
            Some(Transition::Notify(queue)) => match self.queue_notifiers.get(queue as usize) {
                Some(notifier) => notifier.write(1).map_err(TransportError::Notify),
                None => Err(TransportError::Notifiers),
            },
            None => Ok(()),
        };
        if let Err(error) = result {
            warn!("virtio-mmio transition failed: {error}");
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
            features: 1 << 9,
            queue_max: vec![256],
            config: vec![0; 8],
        }
    }

    #[test]
    fn negotiation_requires_version_and_feature_subset() {
        let desc = desc();
        let (regs, effect) = write(MmioRegs::new(&desc), &desc, 0x70, 15);
        assert_eq!((regs.status, effect), (0, None));
        let (regs, _) = write(regs, &desc, 0x70, 1);
        let (regs, _) = write(regs, &desc, 0x70, 3);
        let (regs, _) = write(regs, &desc, 0x70, 11);
        assert_eq!(regs.status, 3);
        let (regs, _) = write(regs, &desc, 0x20, 1);
        let (regs, _) = write(regs, &desc, 0x24, 1);
        let (regs, _) = write(regs, &desc, 0x20, 1);
        let (regs, _) = write(regs, &desc, 0x70, 11);
        assert_eq!(regs.status, 3);
        let (regs, _) = write(regs, &desc, 0x24, 0);
        let (regs, _) = write(regs, &desc, 0x20, 1 << 9);
        let (regs, _) = write(regs, &desc, 0x70, 11);
        let (regs, effect) = write(regs, &desc, 0x70, 15);
        assert_eq!((regs.status, effect), (15, Some(Transition::Activate)));
        let (regs, effect) = write(regs, &desc, 0x70, 0);
        assert_eq!((regs.status, effect), (15, Some(Transition::Reset)));
    }

    #[test]
    fn absent_shm_and_extreme_selectors_are_safe() {
        let desc = desc();
        let (regs, _) = write(MmioRegs::new(&desc), &desc, 0x14, u32::MAX);
        assert_eq!(read(&regs, &desc, 0, 0x10), 0);
        let (regs, _) = write(regs, &desc, 0x30, u32::MAX);
        assert_eq!(read(&regs, &desc, 0, 0x34), 0);
        let (regs, effect) = write(regs, &desc, NOTIFY_OFFSET, u32::MAX);
        assert_eq!(effect, None);
        for offset in [0xb0, 0xb4, 0xb8, 0xbc] {
            assert_eq!(read(&regs, &desc, 0, offset), u32::MAX);
        }
        assert_eq!(read(&regs, &desc, 0, 0xfc), 0);
    }

    #[test]
    fn ready_queue_geometry_is_immutable_and_addresses_are_typed() {
        let desc = desc();
        let regs = MmioRegs {
            status: 11,
            ..MmioRegs::new(&desc)
        };
        let (regs, _) = write(regs, &desc, 0x80, 0x1000);
        let (regs, _) = write(regs, &desc, 0x84, 2);
        assert_eq!(regs.queues[0].desc, Gpa(0x2_0000_1000));
        let (regs, _) = write(regs, &desc, 0x44, 1);
        let (regs, _) = write(regs, &desc, 0x80, 0x2000);
        assert_eq!(regs.queues[0].desc, Gpa(0x2_0000_1000));
    }
}
