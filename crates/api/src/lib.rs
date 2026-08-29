pub mod block;
mod helpers;
pub mod logger;

use crate::block::AddDisk;
use crate::helpers::macros::define_schema;
pub use crate::helpers::op_mode::{self, Reply};
pub use crate::helpers::result::{Error, Result};
use crate::logger::UpdateLogger;

pub type VmmRequest = VmmOp<op_mode::Request>;
pub type VmmCommand = VmmOp<op_mode::Command>;

define_schema! {
    VmmOp {
        UpdateLogger -> Result<()> {
            route: PATCH /v1/logger;
        }
        AddDisk -> Result<()> {
            config: ".machine.disks[]";
            route: PUT /v1/disks;
        }
    }
}
