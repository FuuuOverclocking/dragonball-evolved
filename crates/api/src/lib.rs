pub mod helpers;

pub mod boot;
pub mod config;
pub mod disk;
pub mod instance_info;
pub mod lifecycle;
pub mod live_upgrade;
pub mod logger;
pub mod machine;

use api_macros::define_schema;

use crate::helpers::op_mode;
#[cfg(feature = "request")]
pub use crate::helpers::reply::Reply;
pub use crate::helpers::result::{ApiError, ApiResult};

#[cfg(feature = "request")]
pub type VmmRequest = VmmOp<op_mode::Request>;
pub type VmmCommand = VmmOp<op_mode::Command>;

pub trait RawVmmOp {
    type Response;

    #[cfg(feature = "request")]
    fn into_vmm_request(self) -> (VmmRequest, oneshot::Receiver<Self::Response>);
    fn into_vmm_command(self) -> VmmCommand;
}

define_schema! {
    VmmOp {
        lifecycle::Build -> ApiResult<()> {
            route: POST /v1/build-vm;
        }
        lifecycle::Start -> ApiResult<()> {
            route: POST /v1/start-vm;
        }
        lifecycle::ShutdownVmm -> ApiResult<()> {
            route: POST /v1/shutdown-vmm;
        }
        instance_info::GetInstanceInfo -> instance_info::InstanceInfo {
            route: GET /v1/;
        }
        logger::UpdateLogger -> ApiResult<()> {
            route: PATCH /v1/logger;
        }
        machine::SetCpus -> ApiResult<()> {
            config: ".machine.cpus";
            route: PUT /v1/cpus;
        }
        machine::SetMemoryMib -> ApiResult<()> {
            config: ".machine.memory_mib";
            route: PUT /v1/memory-mib;
        }
        machine::SetConsole -> ApiResult<()> {
            config: ".machine.console";
            route: PUT /v1/console;
        }
        boot::SetBoot -> ApiResult<()> {
            config: ".machine.boot";
            route: PUT /v1/boot;
        }
        disk::AddDisk -> ApiResult<()> {
            config: ".machine.disks[]";
            route: POST /v1/disks;
        }
        live_upgrade::HandoverState -> ApiResult<live_upgrade::LiveUpgradeState> {
            route: POST /v1/live-upgrade/handover-state;
            fd_in: resp;
        }
    }
}
