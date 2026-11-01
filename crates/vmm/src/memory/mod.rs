//! Guest memory: where storage objects sit in the guest ([`layout`]), how the
//! host materializes them ([`policy`]), and the host bytes themselves
//! ([`store`]).

pub mod hotplug;
pub mod layout;
pub mod policy;
pub mod service;
pub mod store;
