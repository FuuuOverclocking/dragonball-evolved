//! What the running machine reports to whoever drives it.

/// An event from a vCPU thread or a platform device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineEvent {
    /// The guest asked for a reset, which ends the machine.
    GuestReset,
    /// A vCPU thread failed; `error` is its rendered source chain.
    VcpuFailed { index: u8, error: String },
}
