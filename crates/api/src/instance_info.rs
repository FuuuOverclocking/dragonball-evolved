#[api_macros::api_type]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GetInstanceInfo;

#[api_macros::api_type]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstanceInfo {
    pub id: String,
    pub version: String,
    pub state: VmState,
}

#[api_macros::api_type]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VmState {
    /// No VM yet; configuration ops are accepted.
    #[default]
    Configuring,

    /// VM built, vCPUs parked at the boot entry; the guest has not run.
    Built,

    /// The guest is running.
    Running,

    /// Waiting for every vCPU to park.
    Pausing,

    /// Every vCPU is parked.
    Paused,

    /// Waiting for every vCPU thread to exit.
    Stopping,

    /// VM resources are released; the VMM is exiting.
    Stopped,
}
