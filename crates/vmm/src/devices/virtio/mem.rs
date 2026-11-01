pub mod protocol;
mod worker;

#[cfg(test)]
mod tests;

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::sync::{mpsc, oneshot};
use virtio_queue::QueueState;
use vmm_sys_util::eventfd::{EFD_CLOEXEC, EFD_NONBLOCK, EventFd};
pub use worker::MemDevice;

use super::config::{ConfigPublisher, ConfigReader, ConfigSnapshot};
use super::mmio::DeviceDesc;
use super::{ActivationRx, Activator, IrqTrigger, activation_channel};
use crate::devices::async_fd::AsyncEventFd;
use crate::memory::hotplug::{MemGeometry, Population, PopulationError};
use crate::memory::service::{
    MemoryOperationError, MemoryOperationId, MemoryPhase, MemoryStatus, RetryError, SubmitError,
    WindowControl, WindowStatus,
};
use crate::space::Range;

const HOST_QUEUE_SIZE: usize = 4;

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum MemError {
    /// create or register virtio-mem notification descriptors
    Notification(#[source] io::Error),
    /// invalid virtio-mem queue configuration
    QueueConfiguration,
    /// invalid virtio-mem negotiated features
    Features(#[from] protocol::FeatureError),
    /// virtio-mem available index exceeds queue capacity
    AvailableIndex,
    /// virtio-mem queue cursors do not match completed chains or the guest used index
    QueuePosition,
    /// access the virtio-mem queue
    Queue(#[from] virtio_queue::Error),
    /// write the virtio-mem response after completing memory work
    Response(#[source] vm_memory::GuestMemoryError),
    /// inject a virtio-mem interrupt
    Interrupt(#[source] io::Error),
    /// invalid virtio-mem configuration
    Config(#[from] protocol::ConfigError),
    /// publish or restore the virtio-mem configuration
    ConfigImage(#[from] super::config::ConfigError),
    /// memory service geometry does not match this virtio-mem device
    GeometryMismatch,
    /// a cold virtio-mem device requires empty population at revision zero
    InitialPopulation,
    /// restored virtio-mem state does not match the service revision or usable population
    RestoreState,
    /// restored active virtio-mem queue must arrive before the worker starts
    RestoreActivation,
    /// incoming virtio-mem queue does not match the saved backend queue
    RestoreQueue,
    /// memory population changed outside the device's accepted operation
    Ownership,
    /// memory service stopped; the virtio-mem session needs reset
    ServiceStopped,
    /// memory service status channel closed; the virtio-mem session needs reset
    StatusClosed,
    /// memory operation {0:?} lost its reply; the virtio-mem session needs reset
    ReplyDropped(MemoryOperationId),
    /// memory operation completion does not match its accepted operation and service projection
    Completion,
    /// memory submission failed; the virtio-mem session needs reset
    Submit(#[source] SubmitError),
    /// memory operation failed; the virtio-mem session needs reset
    Operation(#[source] MemoryOperationError),
}

#[derive(Debug, displaydoc::Display, thiserror::Error)]
pub enum ResizeError {
    /// virtio-mem worker is quiescing or parked
    Paused,
    /// virtio-mem worker has stopped
    Stopped,
    /// virtio-mem host command queue is full
    QueueFull,
    /// invalid virtio-mem requested size
    Config(#[from] protocol::ConfigError),
    /// publish the virtio-mem requested size
    Publication(#[from] super::config::ConfigError),
    /// inject the virtio-mem configuration interrupt
    Interrupt(#[source] io::Error),
    /// retry the stalled memory operation
    Retry(#[from] RetryError),
}

/// Completion of publication (resize) or forwarding (retry), not guest convergence.
/// Dropping this future never withdraws an already enqueued command.
#[derive(Debug)]
pub struct ResizeReply(oneshot::Receiver<Result<(), ResizeError>>);

impl Future for ResizeReply {
    type Output = Result<(), ResizeError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0)
            .poll(cx)
            .map(|result| result.unwrap_or(Err(ResizeError::Stopped)))
    }
}

/// Unique host mutation capability. The worker alone owns the WindowControl.
#[derive(Debug)]
pub struct ResizeHandle {
    resize: mpsc::Sender<ResizeRequest>,
    retry: mpsc::Sender<RetryRequest>,
    admission: Arc<Mutex<ResizeAdmission>>,
}

// Only this enqueue/pause boundary is shared, never device state or the writer.
#[derive(Debug, Default)]
struct ResizeAdmission {
    paused: bool,
    stopped: bool,
}

impl ResizeHandle {
    /// Enqueue without blocking. Await the returned future for publication or a typed rejection.
    pub fn send(&mut self, requested_size: u64) -> Result<ResizeReply, ResizeError> {
        let admission = self
            .admission
            .lock()
            .expect("virtio-mem admission lock poisoned");
        if admission.stopped || self.resize.is_closed() {
            return Err(ResizeError::Stopped);
        }
        if admission.paused {
            return Err(ResizeError::Paused);
        }
        let (reply, answer) = oneshot::channel();
        self.resize
            .try_send(ResizeRequest {
                requested_size,
                reply,
            })
            .map_err(host_send_error)?;
        Ok(ResizeReply(answer))
    }

    /// Still available while quiesce/reset/stop drains a stalled accepted memory operation.
    pub fn retry(&mut self, operation_id: MemoryOperationId) -> Result<ResizeReply, ResizeError> {
        let (reply, answer) = oneshot::channel();
        self.retry
            .try_send(RetryRequest {
                operation_id,
                reply,
            })
            .map_err(host_send_error)?;
        Ok(ResizeReply(answer))
    }
}

fn host_send_error<T>(error: mpsc::error::TrySendError<T>) -> ResizeError {
    match error {
        mpsc::error::TrySendError::Full(_) => ResizeError::QueueFull,
        mpsc::error::TrySendError::Closed(_) => ResizeError::Stopped,
    }
}

#[derive(Debug)]
struct ResizeRequest {
    requested_size: u64,
    reply: oneshot::Sender<Result<(), ResizeError>>,
}

#[derive(Debug)]
struct RetryRequest {
    operation_id: MemoryOperationId,
    reply: oneshot::Sender<Result<(), ResizeError>>,
}

#[derive(Debug)]
pub struct MemHandle {
    pub activator: Activator,
    pub queue_notifier: EventFd,
    pub interrupt: Arc<IrqTrigger>,
    /// Cold immutable identity, including zeroed mutable config fields on restore.
    pub desc: DeviceDesc,
    pub config: ConfigReader,
    pub resize: ResizeHandle,
}

/// Worker-owned state only. Population and bindings belong to the memory service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemQuiesced {
    pub requested_size: u64,
    pub usable_region_size: u64,
    pub memory_revision: u64,
    /// Backend ownership, not the transport's DRIVER_OK bit.
    pub queue: Option<QueueState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemRestore {
    pub state: MemQuiesced,
    pub config: ConfigSnapshot,
}

pub struct MemResources {
    geometry: MemGeometry,
    window: WindowControl,
    status: WindowStatus,
    publisher: ConfigPublisher,
    interrupt: Arc<IrqTrigger>,
    queue_kick: EventFd,
    activation: ActivationRx,
    resize: mpsc::Receiver<ResizeRequest>,
    retry: mpsc::Receiver<RetryRequest>,
    admission: Arc<Mutex<ResizeAdmission>>,
    state: MemQuiesced,
    restored_queue: Option<QueueState>,
}

impl MemResources {
    pub fn new(
        geometry: MemGeometry,
        window: WindowControl,
        status: WindowStatus,
    ) -> Result<(Self, MemHandle), MemError> {
        let snapshot = status.snapshot();
        if snapshot.population.geometry() != geometry {
            return Err(MemError::GeometryMismatch);
        }
        if snapshot.revision != 0 || snapshot.population.plugged_size() != 0 {
            return Err(MemError::InitialPopulation);
        }
        let state = MemQuiesced {
            requested_size: 0,
            usable_region_size: 0,
            memory_revision: 0,
            queue: None,
        };
        let bytes = project(geometry, state, &snapshot)?.encode().to_vec();
        let (publisher, config) = ConfigPublisher::new(bytes);
        Self::prepare(geometry, window, status, publisher, config, state, None)
    }

    /// The integration owner restores the service first and queues the saved Activation
    /// before running this worker parked. This constructor does not activate or inject.
    pub fn restore(
        geometry: MemGeometry,
        window: WindowControl,
        status: WindowStatus,
        restored: MemRestore,
    ) -> Result<(Self, MemHandle), MemError> {
        let snapshot = status.snapshot();
        let image =
            Self::validate_restore(geometry, &snapshot.population, snapshot.revision, &restored)?;
        let (publisher, config) = ConfigPublisher::restore(restored.config, &image.encode())?;
        Self::prepare(
            geometry,
            window,
            status,
            publisher,
            config,
            restored.state,
            restored.state.queue,
        )
    }

    pub(crate) fn descriptor(geometry: MemGeometry) -> DeviceDesc {
        DeviceDesc {
            device_type: protocol::DEVICE_ID,
            features: protocol::OFFERED_FEATURES,
            required_features: protocol::REQUIRED_FEATURES,
            queue_max: vec![protocol::QUEUE_MAX],
            config: baseline(geometry).encode().to_vec(),
        }
    }

    pub(crate) fn validate_restore(
        geometry: MemGeometry,
        population: &Population,
        revision: u64,
        restored: &MemRestore,
    ) -> Result<protocol::Config, MemError> {
        let image = project_population(geometry, restored.state, population)?;
        if restored.state.memory_revision != revision
            || !restored
                .state
                .usable_region_size
                .is_multiple_of(geometry.chunk_size())
        {
            return Err(MemError::RestoreState);
        }
        let bytes = image.encode();
        if restored.config.bytes.len() != bytes.len() {
            return Err(super::config::ConfigError::Length {
                expected: bytes.len(),
                actual: restored.config.bytes.len(),
            }
            .into());
        }
        if restored.config.bytes != bytes {
            return Err(super::config::ConfigError::Image.into());
        }
        Ok(image)
    }

    fn prepare(
        geometry: MemGeometry,
        window: WindowControl,
        status: WindowStatus,
        publisher: ConfigPublisher,
        config: ConfigReader,
        state: MemQuiesced,
        restored_queue: Option<QueueState>,
    ) -> Result<(Self, MemHandle), MemError> {
        if matches!(
            status.snapshot().phase,
            MemoryPhase::Stopping | MemoryPhase::Stopped
        ) {
            return Err(MemError::ServiceStopped);
        }
        let queue_kick =
            EventFd::new(EFD_NONBLOCK | EFD_CLOEXEC).map_err(MemError::Notification)?;
        let queue_notifier = queue_kick.try_clone().map_err(MemError::Notification)?;
        let interrupt = Arc::new(IrqTrigger::new().map_err(MemError::Notification)?);
        let (activator, activation) = activation_channel();
        let (resize_tx, resize) = mpsc::channel(HOST_QUEUE_SIZE);
        let (retry_tx, retry) = mpsc::channel(HOST_QUEUE_SIZE);
        let admission = Arc::new(Mutex::new(ResizeAdmission::default()));
        let desc = Self::descriptor(geometry);
        Ok((
            Self {
                geometry,
                window,
                status,
                publisher,
                interrupt: Arc::clone(&interrupt),
                queue_kick,
                activation,
                resize,
                retry,
                admission: Arc::clone(&admission),
                state,
                restored_queue,
            },
            MemHandle {
                activator,
                queue_notifier,
                interrupt,
                desc,
                config,
                resize: ResizeHandle {
                    resize: resize_tx,
                    retry: retry_tx,
                    admission,
                },
            },
        ))
    }

    /// Call inside the I/O LocalRuntime's factory; resources are transferable, the
    /// running device owns its reactor registration and all mutable state directly.
    pub fn into_device(self) -> Result<MemDevice, MemError> {
        let kick = AsyncEventFd::from_event_fd(self.queue_kick).map_err(MemError::Notification)?;
        Ok(MemDevice::new(
            self.geometry,
            self.window,
            self.status,
            self.publisher,
            self.interrupt,
            kick,
            self.activation,
            self.resize,
            self.retry,
            self.admission,
            self.state,
            self.restored_queue,
        ))
    }
}

fn baseline(geometry: MemGeometry) -> protocol::Config {
    protocol::Config {
        block_size: geometry.block_size(),
        node_id: 0,
        addr: geometry.region().start(),
        region_size: geometry.region().len(),
        usable_region_size: 0,
        plugged_size: 0,
        requested_size: 0,
    }
}

fn project(
    geometry: MemGeometry,
    state: MemQuiesced,
    status: &MemoryStatus,
) -> Result<protocol::Config, MemError> {
    project_population(geometry, state, &status.population)
}

fn project_population(
    geometry: MemGeometry,
    state: MemQuiesced,
    population: &Population,
) -> Result<protocol::Config, MemError> {
    if population.geometry() != geometry {
        return Err(MemError::GeometryMismatch);
    }
    let config = protocol::Config {
        requested_size: state.requested_size,
        usable_region_size: state.usable_region_size,
        plugged_size: population.plugged_size(),
        ..baseline(geometry)
    };
    config.validate()?;
    let usable = Range::new(config.addr, config.usable_region_size)
        .ok_or(protocol::ConfigError::RegionOverflow)?;
    if population
        .plugged_ranges()
        .any(|range| !usable.contains_range(range))
    {
        return Err(MemError::RestoreState);
    }
    Ok(config)
}

fn population_response(error: PopulationError) -> protocol::Response {
    match error {
        PopulationError::TargetExceeded { .. } => protocol::Response::Nack,
        _ => protocol::Response::Error,
    }
}
