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
    /// Vm not started (yet)
    #[default]
    NotStarted,

    /// Vm is Paused
    Paused,

    /// Vm is running
    Running,
}
