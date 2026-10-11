pub mod helpers;

pub mod block;
pub mod config;
pub mod instance_info;
pub mod logger;
pub mod live_upgrade;

use crate::helpers::macros::define_schema;
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
        instance_info::GetInstanceInfo -> instance_info::InstanceInfo {
            route: GET /v1/;
        }
        logger::UpdateLogger -> ApiResult<()> {
            route: PATCH /v1/logger;
        }
        block::AddDisk -> ApiResult<()> {
            config: ".machine.disks[]";
            route: PUT /v1/disks;
        }
        live_upgrade::HandoverState -> ApiResult<live_upgrade::LiveUpgradeState> {
            route: POST /v1/live_upgrade/handover_state;
            fd_in: resp;
        }
    }
}
