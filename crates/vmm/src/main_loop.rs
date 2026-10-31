use std::error::Error;

use api::instance_info::{InstanceInfo, VmState};
use api::logger::UpdateLogger;
use api::{ApiError, ApiResult, Reply, VmmCommand, VmmRequest};

use crate::VmmExitStatus;
use crate::error::render_chain;
use crate::process_ctx::{ProcessCtx, UpdateLoggerFn};
use crate::spec::MachineSpec;
use crate::vmm_server::{Inbound, VmmServer};

#[derive(Debug, displaydoc::Display, thiserror::Error)]
enum Rejection {
    /// starting a machine is not supported yet
    StartUnsupported,
    /// machine is not started
    NotStarted,
    /// live upgrade is not supported
    HandoverUnsupported,
}

/// What request handling needs from the process: everything but KVM.
#[derive(Clone, Copy)]
struct Instance<'a> {
    id: &'a str,
    version: &'a str,
    update_logger: UpdateLoggerFn,
}

enum Step {
    Continue(MachineSpec),
    Exit(VmmExitStatus),
}

pub(crate) async fn run(server: VmmServer, ctx: &ProcessCtx) -> VmmExitStatus {
    let instance = Instance {
        id: &ctx.id,
        version: &ctx.version,
        update_logger: ctx.update_logger,
    };
    serve(server, instance).await
}

async fn serve(mut server: VmmServer, instance: Instance<'_>) -> VmmExitStatus {
    let mut spec = MachineSpec::default();
    loop {
        let request = match server.next_inbound().await {
            Inbound::Request(request) => request,
            // No machine exists yet, so nothing outlives the clients.
            Inbound::Detached => return VmmExitStatus::Ok,
        };
        match handle(spec, request, instance) {
            Step::Continue(next) => spec = next,
            Step::Exit(status) => return status,
        }
    }
}

fn handle(spec: MachineSpec, request: VmmRequest, instance: Instance<'_>) -> Step {
    match request {
        VmmRequest::GetInstanceInfo(_, reply) => reply.send(InstanceInfo {
            id: instance.id.to_owned(),
            version: instance.version.to_owned(),
            state: VmState::NotStarted,
        }),
        VmmRequest::UpdateLogger(UpdateLogger(config), reply) => {
            reply.send((instance.update_logger)(config).map_err(|error| api_error(&*error)));
        }
        VmmRequest::SetCpus(value, reply) => {
            return configure(spec, VmmCommand::SetCpus(value, ()), reply);
        }
        VmmRequest::SetMemory(value, reply) => {
            return configure(spec, VmmCommand::SetMemory(value, ()), reply);
        }
        VmmRequest::SetMemoryBacking(value, reply) => {
            return configure(spec, VmmCommand::SetMemoryBacking(value, ()), reply);
        }
        VmmRequest::SetConsole(value, reply) => {
            return configure(spec, VmmCommand::SetConsole(value, ()), reply);
        }
        VmmRequest::SetKernel(value, reply) => {
            return configure(spec, VmmCommand::SetKernel(value, ()), reply);
        }
        VmmRequest::AddDisk(value, reply) => {
            return configure(spec, VmmCommand::AddDisk(value, ()), reply);
        }
        VmmRequest::Start(_, reply) => reply.send(Err(api_error(&Rejection::StartUnsupported))),
        VmmRequest::Pause(_, reply) | VmmRequest::Resume(_, reply) => {
            reply.send(Err(api_error(&Rejection::NotStarted)));
        }
        VmmRequest::HandoverState(_, reply) => {
            reply.send(Err(api_error(&Rejection::HandoverUnsupported)));
        }
        VmmRequest::Shutdown(_, reply) => {
            reply.send(Ok(()));
            return Step::Exit(VmmExitStatus::Ok);
        }
    }
    Step::Continue(spec)
}

fn configure(spec: MachineSpec, command: VmmCommand, reply: Reply<ApiResult<()>>) -> Step {
    match spec.apply(command) {
        Ok(next) => {
            reply.send(Ok(()));
            Step::Continue(next)
        }
        Err(error) => {
            reply.send(Err(api_error(&error)));
            Step::Continue(spec)
        }
    }
}

fn api_error(error: &dyn Error) -> ApiError {
    ApiError::msg(render_chain(error))
}

#[cfg(test)]
mod tests {
    use std::io;

    use api::RawVmmOp;
    use api::block::{AddDisk, BlockDevConfig};
    use api::instance_info::GetInstanceInfo;
    use api::lifecycle::{Pause, Resume, Shutdown, Start};
    use api::live_upgrade::HandoverState;
    use api::logger::LoggerConfig;
    use api::machine::{SetCpus, VcpuCount};

    use super::*;
    use crate::spec::SpecError;
    use crate::vmm_server::{VmmClient, pair};

    const INSTANCE: Instance<'static> = Instance {
        id: "test-id",
        version: "test-version",
        update_logger: |_| Ok(()),
    };

    fn send<Op: RawVmmOp>(client: &VmmClient, op: Op) -> oneshot::Receiver<Op::Response> {
        let (request, reply) = op.into_vmm_request();
        client.send(request).unwrap();
        reply
    }

    fn root_disk() -> AddDisk {
        AddDisk(BlockDevConfig {
            id: None,
            path: "/test/disk".into(),
            read_only: false,
            root: true,
        })
    }

    fn rejected(error: impl Error) -> Result<ApiResult<()>, oneshot::TryRecvError> {
        Ok(Err(api_error(&error)))
    }

    #[tokio::test(flavor = "local")]
    async fn configuration_accumulates_and_reports_rule_violations() {
        let (client, server) = pair();
        let replies = [
            send(&client, SetCpus(VcpuCount(2))),
            send(&client, SetCpus(VcpuCount(0))),
            send(&client, root_disk()),
            send(&client, root_disk()),
        ];
        drop(client);
        assert_eq!(serve(server, INSTANCE).await, VmmExitStatus::Ok);

        let [cpus, no_cpus, disk, second_root] = replies;
        assert_eq!(cpus.try_recv(), Ok(Ok(())));
        assert_eq!(no_cpus.try_recv(), rejected(SpecError::VcpuCount(0)));
        assert_eq!(disk.try_recv(), Ok(Ok(())));
        assert_eq!(second_root.try_recv(), rejected(SpecError::SecondRootDisk));
    }

    #[tokio::test(flavor = "local")]
    async fn lifecycle_requests_are_rejected_until_shutdown() {
        let (client, server) = pair();
        let info = send(&client, GetInstanceInfo);
        let start = send(&client, Start);
        let pause = send(&client, Pause);
        let resume = send(&client, Resume);
        let handover = send(&client, HandoverState {});
        let shutdown = send(&client, Shutdown);
        let late = send(&client, GetInstanceInfo);
        assert_eq!(serve(server, INSTANCE).await, VmmExitStatus::Ok);

        let info = info.try_recv().unwrap();
        assert_eq!(
            (info.id.as_str(), info.version.as_str(), info.state),
            ("test-id", "test-version", VmState::NotStarted)
        );
        assert_eq!(start.try_recv(), rejected(Rejection::StartUnsupported));
        assert_eq!(pause.try_recv(), rejected(Rejection::NotStarted));
        assert_eq!(resume.try_recv(), rejected(Rejection::NotStarted));
        assert_eq!(
            handover.try_recv().unwrap().unwrap_err(),
            api_error(&Rejection::HandoverUnsupported)
        );
        assert_eq!(shutdown.try_recv(), Ok(Ok(())));
        assert_eq!(late.try_recv(), Err(oneshot::TryRecvError::Disconnected));
    }

    /// apply the logger configuration
    #[derive(Debug, displaydoc::Display, thiserror::Error)]
    struct LoggerFailed(#[source] io::Error);

    #[tokio::test(flavor = "local")]
    async fn logger_updates_report_their_whole_error_chain() {
        let instance = Instance {
            update_logger: |_| Err(LoggerFailed(io::Error::other("target is gone")).into()),
            ..INSTANCE
        };
        let (client, server) = pair();
        let update = send(&client, UpdateLogger(LoggerConfig::default()));
        drop(client);
        assert_eq!(serve(server, instance).await, VmmExitStatus::Ok);
        assert_eq!(
            update.try_recv(),
            Ok(Err(ApiError::msg(
                "apply the logger configuration: target is gone"
            )))
        );
    }

    #[tokio::test(flavor = "local")]
    async fn losing_every_client_before_start_exits_cleanly() {
        let (client, server) = pair();
        drop(client);
        assert_eq!(serve(server, INSTANCE).await, VmmExitStatus::Ok);
    }
}
