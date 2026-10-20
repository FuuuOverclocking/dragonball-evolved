use api::ApiResult;
use api::logger::UpdateLogger;

use crate::kvm_context::KvmContext;

pub struct ProcessCtx {
    pub id: String,
    pub version: String,
    pub kvm_ctx: KvmContext,
    pub update_logger: fn(UpdateLogger) -> ApiResult<()>,
}
