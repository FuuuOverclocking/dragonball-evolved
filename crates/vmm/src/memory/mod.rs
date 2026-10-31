//! Guest memory: where storage objects sit in the guest ([`layout`]), how the
//! host materializes them ([`policy`]), and the host bytes themselves
//! ([`store`]).

pub mod layout;
pub mod policy;
pub mod store;
