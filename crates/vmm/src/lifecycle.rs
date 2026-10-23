use api::{ApiResult, Reply, instance_info::VmState};

#[allow(clippy::large_enum_variant)]
pub(crate) enum Controller {
    /// No lifecycle operation is running; the Vm owns the Machine.
    Idle(Vm),
    /// A lifecycle operation is in progress; the future owns the Machine.
    Busy {
        /// The state the Vm was in before the operation started.
        stable: VmState,
        /// The in-flight operation.
        operation: Operation,
    },
}

pub(crate) enum Vm {
    NotStarted(MachineSpec),
    Running(Machine),
    Paused(Machine),
}

pub type BoxFuture<T> = std::pin::Pin<Box<dyn Future<Output = T> + 'static>>;

pub(crate) struct Operation {
    pub future: BoxFuture<OperationCompletion>,
    pub reply: Option<Reply<ApiResult<()>>>,
    pub cancel_tx: Option<oneshot::Sender<()>>,
}

pub(crate) struct OperationCompletion {
    pub machine: Machine,
    pub target_state: VmState,
    pub result: anyhow::Result<()>,
}

impl Controller {
    pub(crate) fn new(spec: MachineSpec) -> Self {
        Self::Idle(spec)
    }
}
