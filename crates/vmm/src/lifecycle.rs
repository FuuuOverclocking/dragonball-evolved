use api::instance_info::{InstanceInfo, VmState};
use api::{ApiError, ApiResult, VmmCommand, VmmRequest};
use logger::{info_unlimited, warn_unlimited};
use tokio::select;

use crate::devices::DeviceExit;
use crate::machine::MachineEvent;
use crate::machine::build::{Machine, build};
use crate::machine::plan::MachinePlan;
use crate::spec::MachineSpec;
use crate::{ProcessCtx, VmmExitStatus};

pub(crate) enum Vm {
    NotStarted(MachineSpec),
    Running(Machine),
    Paused(Machine),
}

pub(crate) enum Event {
    Request(Option<VmmRequest>),
    Machine(MachineEvent),
    Device(DeviceExit),
}

pub(crate) enum Step {
    Continue,
    Stop(VmmExitStatus),
}

impl Step {
    fn error(error: &anyhow::Error) -> Self {
        Self::Stop(VmmExitStatus::Error(format!("{error:#}")))
    }
}

fn accept_shutdown(reply: api::Reply<ApiResult<()>>) -> Step {
    reply.send(Ok(()));
    Step::Stop(VmmExitStatus::Ok)
}

impl Vm {
    pub(crate) async fn next_event(&mut self) -> Event {
        match self {
            Self::NotStarted(_) => std::future::pending().await,
            Self::Running(machine) | Self::Paused(machine) => select! {
                Some(event) = machine.events.recv() => Event::Machine(event),
                Some(exit) = machine.devices.next_exit() => Event::Device(exit),
            },
        }
    }

    fn state(&self) -> VmState {
        match self {
            Self::NotStarted(_) => VmState::NotStarted,
            Self::Running(_) => VmState::Running,
            Self::Paused(_) => VmState::Paused,
        }
    }

    fn configure(self, command: VmmCommand) -> (Self, ApiResult<()>) {
        match self {
            Self::NotStarted(spec) => match spec.apply(command) {
                Ok(next) => (Self::NotStarted(next), Ok(())),
                Err(error) => (
                    Self::NotStarted(spec),
                    Err(ApiError::msg(format!("{error:#}"))),
                ),
            },
            vm => (
                vm,
                Err(ApiError::msg(
                    "machine configuration cannot change after Start",
                )),
            ),
        }
    }

    pub(crate) async fn handle(self, event: Event, ctx: &ProcessCtx) -> (Self, Step) {
        let request = match event {
            Event::Request(Some(request)) => request,
            Event::Request(None) | Event::Machine(MachineEvent::GuestReset) => {
                return (self, Step::Stop(VmmExitStatus::Ok));
            }
            Event::Machine(MachineEvent::VcpuFailed { index, error }) => {
                return (
                    self,
                    Step::Stop(VmmExitStatus::Error(format!("vCPU {index}: {error}"))),
                );
            }
            Event::Device(exit) => {
                match exit.result {
                    Ok(()) => info_unlimited!("device {} stopped", exit.name),
                    Err(error) => warn_unlimited!("device {} failed: {error:#}", exit.name),
                }
                return (self, Step::Continue);
            }
        };
        match request {
            VmmRequest::GetInstanceInfo(_, reply) => {
                reply.send(InstanceInfo {
                    id: ctx.id.clone(),
                    version: ctx.version.clone(),
                    state: self.state(),
                });
                (self, Step::Continue)
            }
            VmmRequest::UpdateLogger(cfg, reply) => {
                reply.send((ctx.update_logger)(cfg.0));
                (self, Step::Continue)
            }
            VmmRequest::HandoverState(_, reply) => {
                reply.send(Err(ApiError::msg("live upgrade is not supported")));
                (self, Step::Continue)
            }
            VmmRequest::Shutdown(_, reply) => (self, accept_shutdown(reply)),
            VmmRequest::SetCpus(value, reply) => {
                let (next, result) = self.configure(VmmCommand::SetCpus(value, ()));
                reply.send(result);
                (next, Step::Continue)
            }
            VmmRequest::SetMemory(value, reply) => {
                let (next, result) = self.configure(VmmCommand::SetMemory(value, ()));
                reply.send(result);
                (next, Step::Continue)
            }
            VmmRequest::SetConsole(value, reply) => {
                let (next, result) = self.configure(VmmCommand::SetConsole(value, ()));
                reply.send(result);
                (next, Step::Continue)
            }
            VmmRequest::SetKernel(value, reply) => {
                let (next, result) = self.configure(VmmCommand::SetKernel(value, ()));
                reply.send(result);
                (next, Step::Continue)
            }
            VmmRequest::AddDisk(value, reply) => {
                let (next, result) = self.configure(VmmCommand::AddDisk(value, ()));
                reply.send(result);
                (next, Step::Continue)
            }
            VmmRequest::Start(_, reply) => match self {
                Self::NotStarted(spec) => {
                    let result = MachinePlan::new(spec.clone(), ctx.kvm_ctx.capabilities())
                        .and_then(|plan| build(plan, &ctx.kvm_ctx));
                    match result {
                        Ok(machine) => match machine.vcpus.as_ref().unwrap().resume().await {
                            Ok(()) => {
                                reply.send(Ok(()));
                                (Self::Running(machine), Step::Continue)
                            }
                            Err(error) => {
                                let _ = machine.teardown().await;
                                reply.send(Err(ApiError::msg(format!("start vCPUs: {error:#}"))));
                                (Self::NotStarted(spec), Step::Continue)
                            }
                        },
                        Err(error) => {
                            reply.send(Err(ApiError::msg(format!("{error:#}"))));
                            (Self::NotStarted(spec), Step::Continue)
                        }
                    }
                }
                vm => {
                    reply.send(Err(ApiError::msg("VM is already started")));
                    (vm, Step::Continue)
                }
            },
            VmmRequest::Pause(_, reply) => match self {
                Self::Running(machine) => match machine.vcpus.as_ref().unwrap().pause().await {
                    Ok(()) => {
                        reply.send(Ok(()));
                        (Self::Paused(machine), Step::Continue)
                    }
                    Err(error) => {
                        reply.send(Err(ApiError::msg(format!("{error:#}"))));
                        (Self::Running(machine), Step::error(&error))
                    }
                },
                Self::Paused(machine) => {
                    reply.send(Ok(()));
                    (Self::Paused(machine), Step::Continue)
                }
                vm => {
                    reply.send(Err(ApiError::msg("VM is not started")));
                    (vm, Step::Continue)
                }
            },
            VmmRequest::Resume(_, reply) => match self {
                Self::Paused(machine) => match machine.vcpus.as_ref().unwrap().resume().await {
                    Ok(()) => {
                        reply.send(Ok(()));
                        (Self::Running(machine), Step::Continue)
                    }
                    Err(error) => {
                        reply.send(Err(ApiError::msg(format!("{error:#}"))));
                        (Self::Paused(machine), Step::error(&error))
                    }
                },
                Self::Running(machine) => {
                    reply.send(Ok(()));
                    (Self::Running(machine), Step::Continue)
                }
                vm => {
                    reply.send(Err(ApiError::msg("VM is not started")));
                    (vm, Step::Continue)
                }
            },
        }
    }

    pub(crate) async fn teardown(self) -> anyhow::Result<()> {
        match self {
            Self::NotStarted(_) => Ok(()),
            Self::Running(machine) | Self::Paused(machine) => machine.teardown().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use api::RawVmmOp;
    use api::lifecycle::Shutdown;

    use super::*;

    #[test]
    fn shutdown_reply_means_request_accepted() {
        let (request, receiver) = Shutdown.into_vmm_request();
        let VmmRequest::Shutdown(_, reply) = request else {
            panic!("expected Shutdown request");
        };

        assert!(matches!(
            accept_shutdown(reply),
            Step::Stop(VmmExitStatus::Ok)
        ));
        assert_eq!(receiver.try_recv(), Ok(Ok(())));
    }
}
