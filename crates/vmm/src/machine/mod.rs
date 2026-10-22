pub mod build;
pub mod bus;
pub mod memory;
pub mod plan;
pub mod vcpu;

#[derive(Debug)]
pub enum MachineEvent {
    GuestReset,
    VcpuFailed { index: u8, error: String },
}
