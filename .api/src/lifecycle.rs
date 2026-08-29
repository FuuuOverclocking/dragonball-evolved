//! The operations of a vmm's lifetime, and what they answer with.

use serde::{Deserialize, Serialize};

/// Stop the vmm. Answered once every device has quiesced, so the reply means
/// shutdown finished rather than shutdown started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shutdown {}

/// What the vmm is currently doing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {}

/// Which instance this vmm is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetInstanceInfo {}

/// What the vmm is currently doing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmmStatus {
    /// Device tasks that have not reported an exit yet.
    pub devices_running: usize,
}

/// Which instance a vmm is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceInfo {
    /// The instance's id, as the caller named it.
    pub id: String,
}
