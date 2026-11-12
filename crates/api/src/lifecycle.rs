/// Builds the VM from the current configuration without running any vCPU.
#[api_macros::api_type]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Build;

/// Builds the VM from the current configuration (if not yet), then boots it.
#[api_macros::api_type]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Start;

#[api_macros::api_type]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShutdownVmm;
