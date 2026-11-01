use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{io, mem};

use logger::warn;
use virtio_queue::{Queue, QueueT};
use vmm_sys_util::eventfd::EventFd;

use super::config::{ConfigError, ConfigReader, ConfigSnapshot};
use super::interrupt::{InterruptStatusError, VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING};
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
const DRIVER_OK: u32 = 4;
const FEATURES_OK: u32 = 8;
pub const NEEDS_RESET: u32 = 64;
const FAILED: u32 = 128;
const RESET_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDesc {
    pub device_type: u32,
    pub features: u64,
    pub required_features: u64,
    pub queue_max: Vec<u16>,
    pub config: Vec<u8>,
}

impl DeviceDesc {
    fn accepts_features(&self, driver_features: u64) -> bool {
        driver_features & !self.features == 0
            && driver_features & self.required_features == self.required_features
    }
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
    pub config: ConfigSnapshot,
    pub regs: MmioRegs,
    pub interrupt_status: u32,
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum RestoreError {
    /// saved virtio device descriptor does not match the current device
    Descriptor,
    /// restore virtio device configuration
    Config(#[from] ConfigError),
    /// saved virtio queue count does not match the device
    QueueCount,
    /// queue notifier count does not match the device
    Notifiers,
    /// invalid saved virtio device status {0:#x}
    Status(u32),
    /// saved virtio driver features were not validly negotiated
    Features,
    /// invalid saved virtio queue {index}
    Queue {
        index: usize,
        #[source]
        error: QueueError,
    },
    /// restore virtio interrupt status
    Interrupt(#[source] InterruptStatusError),
}

impl MmioTransportSnapshot {
    pub(crate) fn validate(
        &self,
        desc: &DeviceDesc,
        memory: &MemoryView,
    ) -> Result<(), RestoreError> {
        if self.interrupt_status & !(VIRTIO_MMIO_INT_VRING | VIRTIO_MMIO_INT_CONFIG) != 0 {
            return Err(RestoreError::Interrupt(InterruptStatusError(
                self.interrupt_status,
            )));
        }
        if &self.desc != desc {
            return Err(RestoreError::Descriptor);
        }
        if self.regs.queues.len() != desc.queue_max.len() {
            return Err(RestoreError::QueueCount);
        }
        let status = self.regs.status;
        let allowed = ACKNOWLEDGE | DRIVER | DRIVER_OK | FEATURES_OK | NEEDS_RESET | FAILED;
        if status & !allowed != 0
            || !matches!(status & !(NEEDS_RESET | FAILED), 0 | 1 | 3 | 11 | 15)
        {
            return Err(RestoreError::Status(status));
        }
        if status & FEATURES_OK != 0 && !desc.accepts_features(self.regs.driver_features) {
            return Err(RestoreError::Features);
        }
        for (index, (queue, &max)) in self.regs.queues.iter().zip(&desc.queue_max).enumerate() {
            if queue.ready {
                if status & FEATURES_OK == 0 {
                    return Err(RestoreError::Status(status));
                }
                queue
                    .queue(max, memory)
                    .map_err(|error| RestoreError::Queue { index, error })?;
            }
        }
        Ok(())
    }
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
        0x10 if regs.features_sel < 2 => (desc.features >> (regs.features_sel * 32)) as u32,
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
            if valid && (value & FEATURES_OK == 0 || desc.accepts_features(regs.driver_features)) {
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
    /// configure virtio device configuration
    Config(#[from] ConfigError),
    /// virtio driver features were not validly negotiated
    Features,
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
    config: ConfigReader,
    memory: MemoryView,
    activator: Activator,
    interrupt: Arc<IrqTrigger>,
    queue_notifiers: Vec<EventFd>,
}

impl MmioTransport {
    pub fn new(
        desc: DeviceDesc,
        config: ConfigReader,
        memory: MemoryView,
        activator: Activator,
        interrupt: Arc<IrqTrigger>,
        queue_notifiers: Vec<EventFd>,
    ) -> Result<Self, TransportError> {
        config.validate(&desc.config)?;
        let regs = MmioRegs::new(&desc);
        Ok(Self {
            state: Mutex::new(TransportState {
                regs,
                session: None,
            }),
            desc,
            config,
            memory,
            activator,
            interrupt,
            queue_notifiers,
        })
    }

    /// Capture pure transport state. The caller proves the machine is paused and quiesced.
    pub fn snapshot(&self) -> MmioTransportSnapshot {
        let state = self.state.lock().unwrap();
        MmioTransportSnapshot {
            desc: self.desc.clone(),
            config: self.config.snapshot(),
            regs: state.regs.clone(),
            interrupt_status: self.interrupt.status(),
        }
    }

    /// Pair reset routing with the backend's restored session, without activation or IRQ injection.
    pub fn restore(
        desc: DeviceDesc,
        config: ConfigReader,
        memory: MemoryView,
        activator: Activator,
        interrupt: Arc<IrqTrigger>,
        queue_notifiers: Vec<EventFd>,
        snapshot: MmioTransportSnapshot,
        session: Option<SessionControl>,
    ) -> Result<Self, RestoreError> {
        snapshot.validate(&desc, &memory)?;
        config.validate_restore(&desc.config, &snapshot.config)?;
        if queue_notifiers.len() != desc.queue_max.len() {
            return Err(RestoreError::Notifiers);
        }
        interrupt
            .restore_status(snapshot.interrupt_status)
            .map_err(RestoreError::Interrupt)?;
        Ok(Self {
            state: Mutex::new(TransportState {
                regs: snapshot.regs,
                session,
            }),
            desc,
            config,
            memory,
            activator,
            interrupt,
            queue_notifiers,
        })
    }

    fn activate(&self, state: &mut TransportState) -> Result<(), TransportError> {
        if !self.desc.accepts_features(state.regs.driver_features) {
            return Err(TransportError::Features);
        }
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
            .collect::<Result<Vec<_>, _>>()
            .map_err(TransportError::Queue)?;
        let (control, session) = session_channel();
        self.activator
            .activate(Activation {
                queues,
                mem: Arc::clone(&self.memory),
                interrupt: Arc::clone(&self.interrupt),
                driver_features: state.regs.driver_features,
                session,
            })
            .map_err(TransportError::Activate)?;
        state.session = Some(control);
        for notifier in &self.queue_notifiers {
            notifier.write(1).map_err(TransportError::Notify)?;
        }
        Ok(())
    }

    fn reset(&self, state: &mut TransportState) -> Result<(), TransportError> {
        if let Some(session) = &mut state.session {
            session.reset(RESET_WAIT).map_err(TransportError::Reset)?;
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
            if let Ok(start) = usize::try_from(offset - 0x100) {
                self.config.read(start, data);
            }
        } else if data.len() == 4 && offset.is_multiple_of(4) {
            let value = if offset == 0xfc {
                self.config.generation()
            } else {
                let state = self.state.lock().unwrap();
                read(&state.regs, &self.desc, self.interrupt.status(), offset)
            };
            data.copy_from_slice(&value.to_le_bytes());
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
    use std::error::Error as _;

    use vm_memory::{GuestAddress, GuestMemoryMmap};
    use vmm_sys_util::eventfd::{EFD_CLOEXEC, EFD_NONBLOCK};

    use super::*;
    use crate::devices::virtio::config::ConfigPublisher;
    use crate::devices::virtio::{ActivationRx, activation_channel};
    use crate::error::render_chain;

    #[test]
    fn transport_errors_preserve_typed_sources_and_conversions() {
        let error = TransportError::from(QueueError::Geometry);
        assert!(matches!(
            error.source().unwrap().downcast_ref::<QueueError>(),
            Some(QueueError::Geometry)
        ));
        assert_eq!(
            render_chain(&error),
            "configure the virtio queues: invalid virtio queue size or readiness"
        );

        let error = TransportError::from(ActivationError::Closed);
        assert_eq!(
            error.source().unwrap().downcast_ref::<ActivationError>(),
            Some(&ActivationError::Closed)
        );
        assert_eq!(
            render_chain(&error),
            "send the virtio activation: device activation receiver has exited"
        );

        let error = TransportError::from(ResetError::TimedOut);
        assert_eq!(
            error.source().unwrap().downcast_ref::<ResetError>(),
            Some(&ResetError::TimedOut)
        );
        assert_eq!(
            render_chain(&error),
            "reset the virtio session: device session did not reset in time"
        );

        let error = TransportError::Notify(io::Error::other("notify failure"));
        let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
        assert_eq!(source.kind(), io::ErrorKind::Other);
        assert_eq!(
            render_chain(&error),
            "notify the virtio queue: notify failure"
        );
    }

    #[test]
    fn restore_queue_error_preserves_both_source_levels() {
        let cause = Queue::new(0).unwrap_err();
        let cause_message = cause.to_string();
        let error = RestoreError::Queue {
            index: 2,
            error: QueueError::Create(cause),
        };
        let queue_error = error
            .source()
            .unwrap()
            .downcast_ref::<QueueError>()
            .unwrap();
        assert!(matches!(queue_error, QueueError::Create(_)));
        let cause = queue_error
            .source()
            .unwrap()
            .downcast_ref::<virtio_queue::Error>()
            .unwrap();
        assert_eq!(cause.to_string(), cause_message);
        assert_eq!(error.to_string(), "invalid saved virtio queue 2");
        assert_eq!(
            render_chain(&error),
            format!("invalid saved virtio queue 2: create the virtio queue: {cause_message}")
        );
    }

    #[test]
    fn restore_interrupt_error_preserves_source_without_repeating_cause() {
        let error = RestoreError::Interrupt(InterruptStatusError(4));
        assert_eq!(
            error
                .source()
                .unwrap()
                .downcast_ref::<InterruptStatusError>(),
            Some(&InterruptStatusError(4))
        );
        assert_eq!(error.to_string(), "restore virtio interrupt status");
        assert_eq!(
            render_chain(&error),
            "restore virtio interrupt status: invalid saved virtio interrupt status 0x4"
        );
    }

    fn desc() -> DeviceDesc {
        DeviceDesc {
            device_type: 2,
            features: VERSION_1 | (1 << 9),
            required_features: VERSION_1,
            queue_max: vec![256],
            config: vec![0; 8],
        }
    }

    fn memory() -> MemoryView {
        Arc::new(GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap())
    }

    fn transport(desc: DeviceDesc, config: ConfigReader) -> (MmioTransport, ActivationRx) {
        let (activator, receiver) = activation_channel();
        let transport = MmioTransport::new(
            desc,
            config,
            memory(),
            activator,
            Arc::new(IrqTrigger::new().unwrap()),
            vec![EventFd::new(EFD_NONBLOCK | EFD_CLOEXEC).unwrap()],
        )
        .unwrap();
        (transport, receiver)
    }

    fn restore(
        desc: DeviceDesc,
        config: ConfigReader,
        snapshot: MmioTransportSnapshot,
    ) -> Result<MmioTransport, RestoreError> {
        let (activator, _receiver) = activation_channel();
        MmioTransport::restore(
            desc,
            config,
            memory(),
            activator,
            Arc::new(IrqTrigger::new().unwrap()),
            vec![EventFd::new(EFD_NONBLOCK | EFD_CLOEXEC).unwrap()],
            snapshot,
            None,
        )
    }

    fn reg(transport: &MmioTransport, offset: u64) -> u32 {
        let mut bytes = [0; 4];
        transport.read(offset, &mut bytes);
        u32::from_le_bytes(bytes)
    }

    #[test]
    fn required_feature_mask_is_enforced_at_negotiation_activation_and_restore() {
        const EXTRA: u64 = 1 << 33;
        let mut desc = desc();
        desc.features |= EXTRA;
        desc.required_features |= EXTRA;
        let (transport, mut receiver) =
            transport(desc.clone(), ConfigReader::fixed(desc.config.clone()));
        let regs = MmioRegs {
            features_sel: 1,
            ..MmioRegs::new(&desc)
        };
        assert_eq!(read(&regs, &desc, 0, 0x10), (desc.features >> 32) as u32);
        for driver_features in [0, VERSION_1, EXTRA, VERSION_1 | EXTRA | (1 << 63)] {
            for (status, value) in [(3, 11), (11, 15)] {
                let regs = MmioRegs {
                    status,
                    driver_features,
                    ..MmioRegs::new(&desc)
                };
                let (regs, transition) = write(regs, &desc, 0x70, value);
                assert_eq!(regs.status, status);
                assert_eq!(transition, None);
            }
            for status in [11, 15, 11 | FAILED, 15 | NEEDS_RESET] {
                let mut snapshot = transport.snapshot();
                snapshot.regs.status = status;
                snapshot.regs.driver_features = driver_features;
                assert!(matches!(
                    restore(
                        desc.clone(),
                        ConfigReader::fixed(desc.config.clone()),
                        snapshot,
                    ),
                    Err(RestoreError::Features)
                ));
            }
            let mut state = transport.state.lock().unwrap();
            state.regs.driver_features = driver_features;
            assert!(matches!(
                transport.activate(&mut state),
                Err(TransportError::Features)
            ));
            assert!(state.session.is_none());
            assert!(receiver.try_recv().is_none());
        }
        for driver_features in [VERSION_1 | EXTRA, desc.features] {
            let regs = MmioRegs {
                status: 3,
                driver_features,
                ..MmioRegs::new(&desc)
            };
            let (regs, _) = write(regs, &desc, 0x70, 11);
            let (regs, transition) = write(regs, &desc, 0x70, 15);
            assert_eq!((regs.status, transition), (15, Some(Transition::Activate)));
        }
    }

    #[test]
    fn fixed_configuration_restore_rejects_image_generation_and_length_changes() {
        let desc = desc();
        let reader = ConfigReader::fixed(desc.config.clone());
        let (transport, _receiver) = transport(desc.clone(), reader.clone());
        let saved = transport.snapshot();
        assert_eq!(saved.config.bytes, desc.config);
        assert_eq!(reg(&transport, 0xfc), 0);
        for mutation in 0..3 {
            let mut bad = saved.clone();
            match mutation {
                0 => bad.config.bytes[0] = 1,
                1 => bad.config.generation = 1,
                _ => bad.config.bytes.push(0),
            }
            assert!(matches!(
                restore(desc.clone(), reader.clone(), bad),
                Err(RestoreError::Config(_))
            ));
        }
        assert!(matches!(
            restore(desc, ConfigReader::fixed(vec![1; 8]), saved),
            Err(RestoreError::Config(ConfigError::Image))
        ));
    }

    #[test]
    fn dynamic_configuration_restores_generation_without_activation_or_irq() {
        let desc = desc();
        let (mut publisher, reader) = ConfigPublisher::new(desc.config.clone());
        let (transport, mut receiver) = transport(desc.clone(), reader.clone());
        let image = [7; 8];
        assert_eq!(publisher.publish(&image), Ok(true));
        assert_eq!(reg(&transport, 0xfc), 1);
        let mut bytes = [0; 8];
        transport.read(0x100, &mut bytes);
        assert_eq!(bytes, image);
        assert_eq!(publisher.publish(&image), Ok(false));
        assert_eq!(reg(&transport, 0xfc), 1);
        assert!(receiver.try_recv().is_none());
        assert_eq!(transport.interrupt.status(), 0);
        assert_eq!(
            transport.interrupt.irq_fd().read().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let mut saved = transport.snapshot();
        assert_eq!(saved.desc.config, vec![0; 8]);
        assert_eq!(saved.config.bytes, image);
        saved.interrupt_status = 2;
        let (mut restored_publisher, restored_reader) =
            ConfigPublisher::restore(saved.config.clone(), &image).unwrap();
        let (activator, mut restored_receiver) = activation_channel();
        let interrupt = Arc::new(IrqTrigger::new().unwrap());
        let restored = MmioTransport::restore(
            desc,
            restored_reader,
            memory(),
            activator,
            Arc::clone(&interrupt),
            vec![EventFd::new(EFD_NONBLOCK | EFD_CLOEXEC).unwrap()],
            saved.clone(),
            None,
        )
        .unwrap();
        assert_eq!(restored.snapshot(), saved);
        assert_eq!(reg(&restored, 0xfc), 1);
        restored.read(0x100, &mut bytes);
        assert_eq!(bytes, image);
        assert!(restored_receiver.try_recv().is_none());
        assert_eq!(interrupt.status(), 2);
        assert_eq!(
            interrupt.irq_fd().read().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(restored_publisher.publish(&image), Ok(false));
        assert_eq!(restored_publisher.publish(&[9; 8]), Ok(true));
        assert_eq!(reg(&restored, 0xfc), 2);
        restored.read(0x100, &mut bytes);
        assert_eq!(bytes, [9; 8]);
        assert_eq!(interrupt.status(), 2);
        assert_eq!(
            interrupt.irq_fd().read().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn dynamic_restore_requires_an_already_matching_reader_and_baseline_length() {
        let desc = desc();
        let (mut publisher, reader) = ConfigPublisher::new(desc.config.clone());
        let (transport, _receiver) = transport(desc.clone(), reader.clone());
        publisher.publish(&[1; 8]).unwrap();
        let saved = transport.snapshot();
        let (_publisher, initial_reader) = ConfigPublisher::new(desc.config.clone());
        assert!(matches!(
            restore(desc.clone(), initial_reader, saved.clone()),
            Err(RestoreError::Config(ConfigError::Snapshot))
        ));
        let (_publisher, reset_generation_reader) =
            ConfigPublisher::new(saved.config.bytes.clone());
        assert!(matches!(
            restore(desc.clone(), reset_generation_reader, saved.clone()),
            Err(RestoreError::Config(ConfigError::Snapshot))
        ));
        publisher.publish(&[2; 8]).unwrap();
        assert!(matches!(
            restore(desc.clone(), reader, saved.clone()),
            Err(RestoreError::Config(ConfigError::Snapshot))
        ));
        let mut short = saved;
        short.config.bytes.pop();
        let (_publisher, short_reader) =
            ConfigPublisher::restore(short.config.clone(), &short.config.bytes).unwrap();
        assert!(matches!(
            restore(desc, short_reader, short),
            Err(RestoreError::Config(ConfigError::Length {
                expected: 8,
                actual: 7,
            }))
        ));
    }

    #[test]
    fn configuration_errors_preserve_typed_transport_sources() {
        let error = TransportError::from(ConfigError::Image);
        assert_eq!(
            error.source().unwrap().downcast_ref::<ConfigError>(),
            Some(&ConfigError::Image)
        );
        assert_eq!(
            render_chain(&error),
            "configure virtio device configuration: virtio configuration image does not match the expected bytes"
        );
        let error = RestoreError::from(ConfigError::Snapshot);
        assert_eq!(
            error.source().unwrap().downcast_ref::<ConfigError>(),
            Some(&ConfigError::Snapshot)
        );
        assert_eq!(
            render_chain(&error),
            "restore virtio device configuration: virtio configuration reader does not match the saved state"
        );
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
