use std::error::Error;

use api::logger::LoggerConfig;

use crate::kvm_context::KvmContext;

pub(crate) type UpdateLoggerFn = fn(LoggerConfig) -> Result<(), Box<dyn Error + Send + Sync>>;

pub struct ProcessCtx {
    pub id: String,
    pub version: String,
    pub kvm_ctx: KvmContext,
    pub update_logger: UpdateLoggerFn,
}
