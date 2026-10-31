//! Device emulation.
//!
//! A device is an ordinary async task on a local runtime, on vmm-main or on the
//! I/O thread. It owns its descriptors, its emulation state and its backend
//! outright and decides what to wait for next, so it may hold `!Send` state and
//! its own path takes no locks. What vCPU threads genuinely share, such as
//! registers accessed through a bus or an interrupt line, sits behind handles
//! of its own instead of behind one lock around the whole device.

pub mod async_fd;
pub mod control;
mod io_thread;
pub mod legacy;
pub mod runtime;
pub mod virtio;
