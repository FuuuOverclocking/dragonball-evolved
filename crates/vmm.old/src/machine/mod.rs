pub mod block;
pub mod build;
pub mod bus;
pub mod memory;
pub mod plan;
pub mod platform;
pub mod snapshot;
pub mod vcpu;
pub mod vm;

#[derive(Debug)]
pub enum MachineEvent {
    GuestReset,
    VcpuFailed { index: u8, error: String },
}
