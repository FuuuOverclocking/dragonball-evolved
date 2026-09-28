mod helpers;

pub mod block;
pub mod config;
pub mod logger;

pub use crate::config::{Config, ProcessConfig};
use crate::helpers::macros::define_schema;
use crate::helpers::op_mode;
#[cfg(feature = "request")]
pub use crate::helpers::op_mode::Reply;
pub use crate::helpers::result::{ApiError, ApiResult};

#[cfg(feature = "request")]
pub type VmmRequest = VmmOp<op_mode::Request>;
pub type VmmCommand = VmmOp<op_mode::Command>;

define_schema! {
    VmmOp {
        logger::UpdateLogger -> ApiResult<()> {
            route: PATCH /v1/logger;
        }
        block::AddDisk -> ApiResult<()> {
            config: ".machine.disks[]";
            route: PUT /v1/disks;
        }
    }
}
