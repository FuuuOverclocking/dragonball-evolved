use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::block::AttachedBlock;
use super::mem::AttachedMem;
use super::platform::Platform;
use super::state::{Parts, TeardownError};
use super::vcpu::Vcpus;
use crate::devices::runtime::{DeviceRuntime, Response};
use crate::memory::layout::WindowId;
use crate::memory::service::{MemoryHandle, MemoryPhase, WindowStatus};
use crate::memory::store::MemoryStore;
use crate::vm::{Binding, KvmVm, MemSlots};

#[derive(Debug)]
pub(super) struct CleanupParts {
    pub(super) vcpus: Option<Vcpus>,
    pub(super) runtime: DeviceRuntime,
    pub(super) platform: Option<Platform>,
    pub(super) blocks: BTreeMap<String, AttachedBlock>,
    pub(super) mems: BTreeMap<String, AttachedMem>,
    pub(super) memory_services: BTreeMap<WindowId, MemoryHandle>,
    pub(super) memory_statuses: BTreeMap<WindowId, WindowStatus>,
    pub(super) bindings: Vec<Binding>,
    pub(super) memslots: MemSlots,
    pub(super) vm: Arc<KvmVm>,
}

#[derive(Debug)]
enum Stage {
    Tasks(Box<CleanupParts>),
    Vm(Arc<KvmVm>),
}

/// The sole, retryable owner of a machine that is being stopped.
/// Dropping it requests stop through resource-handle RAII without waiting;
/// it does not prove cleanup completion or return RAM. A live memory service
/// owns its chunk bindings and VM until it stops. Use `wait` to complete cleanup.
#[derive(Debug)]
pub struct PendingCleanup(Stage);

impl PendingCleanup {
    pub(super) fn tasks(parts: Box<CleanupParts>) -> Self {
        Self(Stage::Tasks(parts))
    }

    pub(super) fn from_parts(parts: Box<Parts>) -> Self {
        let Parts {
            vcpus,
            runtime,
            platform,
            blocks,
            mems,
            memory_services,
            bindings,
            memslots,
            vm,
            plan: _,
        } = *parts;
        let memory_statuses = mems
            .values()
            .map(|mem| {
                (
                    mem.status.snapshot().population.geometry().window_id(),
                    mem.status.clone(),
                )
            })
            .collect();
        Self::tasks(Box::new(CleanupParts {
            vcpus: Some(vcpus),
            runtime,
            platform: Some(platform),
            blocks,
            mems,
            memory_services,
            memory_statuses,
            bindings,
            memslots,
            vm,
        }))
    }

    pub(super) fn vm(vm: Arc<KvmVm>) -> Self {
        Self(Stage::Vm(vm))
    }

    /// Stop CPUs, then tasks and the I/O thread, on one deadline. A failure
    /// retains every owner, even when a completed panic is reported only once.
    pub async fn wait(mut self, within: Duration) -> Result<MemoryStore, (Self, TeardownError)> {
        let deadline = Instant::now() + within;
        if let Stage::Tasks(parts) = &mut self.0 {
            let cpus = match &mut parts.vcpus {
                Some(vcpus) => {
                    vcpus
                        .stop_wait(deadline.saturating_duration_since(Instant::now()))
                        .await
                }
                None => Ok(()),
            };
            if let Err(error) = cpus {
                return Err((self, TeardownError::Vcpus(error)));
            }
            for mem in parts.mems.values() {
                mem.worker.stop();
            }
            for service in parts.memory_services.values() {
                service.stop();
            }
            for (window, service) in &parts.memory_services {
                let response = service
                    .stop_wait(deadline.saturating_duration_since(Instant::now()))
                    .await;
                let error = match response {
                    Response::TimedOut => Some(TeardownError::MemoryServiceTimedOut(*window)),
                    Response::Exited => Some(TeardownError::MemoryServiceExited(*window)),
                    Response::Replied(()) => {
                        if matches!(
                            parts.memory_statuses[window].snapshot().phase,
                            MemoryPhase::Stopped
                        ) {
                            None
                        } else {
                            Some(TeardownError::MemoryServiceExited(*window))
                        }
                    }
                };
                if let Some(error) = error {
                    return Err((self, error));
                }
            }
            if let Err(error) = parts
                .runtime
                .shutdown_wait(deadline.saturating_duration_since(Instant::now()))
                .await
            {
                return Err((self, TeardownError::Devices(error)));
            }
        }
        let vm = match self.0 {
            Stage::Vm(vm) => vm,
            Stage::Tasks(parts) => {
                let CleanupParts {
                    vcpus,
                    runtime,
                    platform,
                    blocks,
                    mems,
                    memory_services,
                    memory_statuses,
                    bindings,
                    memslots,
                    vm,
                } = *parts;
                drop(vcpus);
                drop(runtime);
                drop(platform);
                drop(blocks);
                drop(mems);
                drop(memory_services);
                drop(memory_statuses);
                drop(bindings);
                drop(memslots);
                vm
            }
        };
        KvmVm::into_memory(vm).map_err(|vm| (Self::vm(vm), TeardownError::VmHeld))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tokio::sync::oneshot;
    use vm_memory::{Bytes, GuestAddress};

    use super::*;
    use crate::align::BackingPageSize;
    use crate::devices::runtime::{Placement, ShutdownError, Start};
    use crate::kvm_context::KvmContext;
    use crate::memory::layout::MemoryLayout;
    use crate::memory::policy::{BackingKind, HostPolicy};
    use crate::vm::create_vm;

    const TIMEOUT: Duration = Duration::from_secs(5);
    const BACKINGS: [BackingKind; 2] = [BackingKind::AnonymousPrivate, BackingKind::SharedMemfd];

    fn context() -> Option<KvmContext> {
        Path::new("/dev/kvm")
            .exists()
            .then(|| KvmContext::from_dev_path("/dev/kvm").unwrap())
    }

    fn memory(ctx: &KvmContext, kind: BackingKind) -> MemoryStore {
        MemoryStore::materialize(
            MemoryLayout::boot(16 << 20).unwrap(),
            HostPolicy {
                kind,
                page_size: BackingPageSize::base(ctx.capabilities().page_size),
            },
            ctx.capabilities(),
        )
        .unwrap()
    }

    #[tokio::test(flavor = "local")]
    async fn delayed_tasks_retain_the_same_ram_until_a_successful_retry() {
        let Some(ctx) = context() else { return };
        for kind in BACKINGS {
            for placement in [Placement::VmmMain, Placement::Io] {
                let memory = memory(&ctx, kind);
                memory
                    .view()
                    .write_obj(0xab_u8, GuestAddress(0x8000))
                    .unwrap();
                let weak = Arc::downgrade(memory.view());
                let (vm, bindings, memslots) = create_vm(&ctx, memory).unwrap();
                let task_vm = Arc::clone(&vm);
                let (mut runtime, _exits) = DeviceRuntime::new();
                let (entered_tx, entered_rx) = oneshot::channel();
                let (release_tx, release_rx) = oneshot::channel();
                let handle: crate::devices::runtime::DeviceHandle<()> = runtime
                    .spawn_on(
                        "delayed",
                        placement,
                        Start::Running,
                        move |control| async move {
                            entered_tx.send(()).unwrap();
                            release_rx.await.unwrap();
                            drop(task_vm);
                            drop(control);
                            Ok::<(), std::io::Error>(())
                        },
                    )
                    .unwrap();
                entered_rx.await.unwrap();
                let mut pending = PendingCleanup::tasks(Box::new(CleanupParts {
                    vcpus: None,
                    runtime,
                    platform: None,
                    blocks: BTreeMap::new(),
                    mems: BTreeMap::new(),
                    memory_services: BTreeMap::new(),
                    memory_statuses: BTreeMap::new(),
                    bindings,
                    memslots,
                    vm,
                }));
                for _ in 0..2 {
                    let (owner, error) = pending.wait(Duration::ZERO).await.unwrap_err();
                    assert!(matches!(
                        error,
                        TeardownError::Devices(ShutdownError::Unresponsive(_))
                    ));
                    assert!(weak.upgrade().is_some());
                    pending = owner;
                }
                release_tx.send(()).unwrap();
                let memory = pending.wait(TIMEOUT).await.unwrap();
                assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), memory.view()));
                assert_eq!(
                    memory.view().read_obj::<u8>(GuestAddress(0x8000)).unwrap(),
                    0xab
                );
                assert_eq!(memory.policy().kind, kind);
                drop(handle);
                drop(memory);
                assert!(weak.upgrade().is_none());
            }
        }
    }

    #[tokio::test(flavor = "local")]
    async fn vm_only_cleanup_retains_the_arc_across_repeated_unwrap_failures() {
        let Some(ctx) = context() else { return };
        for kind in BACKINGS {
            let (vm, bindings, slots) = create_vm(&ctx, memory(&ctx, kind)).unwrap();
            let held = Arc::clone(&vm);
            let weak = Arc::downgrade(vm.memory().view());
            drop(bindings);
            drop(slots);
            let mut pending = PendingCleanup::vm(vm);
            for _ in 0..2 {
                let (owner, error) = pending.wait(Duration::ZERO).await.unwrap_err();
                assert!(matches!(error, TeardownError::VmHeld));
                assert!(weak.upgrade().is_some());
                pending = owner;
            }
            drop(held);
            let memory = pending.wait(TIMEOUT).await.unwrap();
            assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), memory.view()));
            drop(memory);
            assert!(weak.upgrade().is_none());
        }
    }

    #[tokio::test(flavor = "local")]
    async fn cpu_timeout_preserves_device_owners_without_requesting_stop_before_retry() {
        use std::sync::{Mutex, mpsc};

        use super::super::vcpu::{self, Buses, VcpuError};
        use crate::bus::{BusDevice, MmioBus, PioBus};
        use crate::devices::control::Command;
        use crate::devices::runtime::DeviceHandle;
        use crate::space::{Gpa, Pio, Range};

        struct Gate {
            entered: Mutex<Option<oneshot::Sender<()>>>,
            release: Mutex<mpsc::Receiver<()>>,
            panics: bool,
        }
        impl BusDevice<Pio> for Gate {
            fn read(&self, _: u16, data: &mut [u8]) {
                data.fill(0);
            }
            fn write(&self, _: u16, _: &[u8]) {
                self.entered
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(())
                    .unwrap();
                self.release.lock().unwrap().recv().unwrap();
                assert!(!self.panics, "injected vCPU panic after delayed release");
            }
        }
        let Some(ctx) = context() else { return };
        for (kind, panics) in BACKINGS
            .into_iter()
            .flat_map(|kind| [false, true].map(|panics| (kind, panics)))
        {
            let memory = memory(&ctx, kind);
            memory
                .view()
                .write_slice(
                    &[0xba, 0x00, 0x03, 0x00, 0x00, 0xb0, 1, 0xee, 0xeb, 0xfe],
                    GuestAddress(0x1000),
                )
                .unwrap();
            let weak = Arc::downgrade(memory.view());
            let (vm, bindings, memslots) = create_vm(&ctx, memory).unwrap();
            let cpu = vm.create_vcpu(0).unwrap();
            vcpu::configure(
                &cpu,
                ctx.capabilities(),
                1,
                0,
                Gpa(0x1000),
                vm.memory().view(),
            )
            .unwrap();
            let (entered_tx, entered_rx) = oneshot::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let gate: Arc<dyn BusDevice<Pio>> = Arc::new(Gate {
                entered: Mutex::new(Some(entered_tx)),
                release: Mutex::new(release_rx),
                panics,
            });
            let buses = Arc::new(Buses {
                pio: PioBus::new(vec![(Range::new(Pio(0x300), 1).unwrap(), gate)]).unwrap(),
                mmio: MmioBus::default(),
            });
            let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
            let mut vcpus = Vcpus::spawn_retained(vec![cpu], buses, events).unwrap();
            vcpus.resume(TIMEOUT).await.unwrap();
            tokio::time::timeout(TIMEOUT, entered_rx)
                .await
                .unwrap()
                .unwrap();
            let (mut runtime, _exits) = DeviceRuntime::new();
            let (stopped_tx, mut stopped_rx) = oneshot::channel();
            let handle: DeviceHandle<()> = runtime
                .spawn(
                    "stop-observer",
                    Start::Running,
                    move |mut control| async move {
                        assert!(matches!(control.recv().await, Command::Stop));
                        stopped_tx.send(()).unwrap();
                        Ok::<(), std::io::Error>(())
                    },
                )
                .unwrap();
            let pending = PendingCleanup::tasks(Box::new(CleanupParts {
                vcpus: Some(vcpus),
                runtime,
                platform: None,
                blocks: BTreeMap::new(),
                mems: BTreeMap::new(),
                memory_services: BTreeMap::new(),
                memory_statuses: BTreeMap::new(),
                bindings,
                memslots,
                vm,
            }));
            let (pending, error) = pending.wait(Duration::ZERO).await.unwrap_err();
            assert!(matches!(
                error,
                TeardownError::Vcpus(VcpuError::TimedOut(0))
            ));
            assert!(matches!(
                stopped_rx.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            assert!(!handle.has_exited());
            assert!(weak.upgrade().is_some());
            release_tx.send(()).unwrap();
            let pending = if panics {
                let (pending, error) = pending.wait(TIMEOUT).await.unwrap_err();
                assert!(matches!(
                    error,
                    TeardownError::Vcpus(VcpuError::Panicked(0))
                ));
                pending
            } else {
                pending
            };
            let memory = pending.wait(TIMEOUT).await.unwrap();
            tokio::time::timeout(TIMEOUT, stopped_rx)
                .await
                .unwrap()
                .unwrap();
            assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), memory.view()));
            drop(handle);
            drop(memory);
            assert!(weak.upgrade().is_none());
        }
    }
}
