#[cfg(feature = "runtime")]
pub mod cli;
pub mod config;
#[cfg(feature = "runtime")]
pub mod init;
#[cfg(feature = "schema")]
pub mod schema;

pub const VERSION: &str = env!("VERSION");
pub const VERSION_LONG: &str = env!("VERSION_LONG");
pub const COMMIT: &str = env!("GIT_SHORT_HASH");
