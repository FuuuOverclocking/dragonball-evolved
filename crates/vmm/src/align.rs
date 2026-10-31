//! Alignments, and the page sizes that impose them.
//!
//! The guest's page belongs to its ABI (e820 / PVH memory map, boot data); the
//! host's page governs mappings, and the backing's page the granule of guest
//! memory on the host. They are different types so that one cannot stand in
//! for another.

use std::fmt;

/// A power of two that points and lengths can be aligned to.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Alignment(u64);

impl Alignment {
    /// `None` unless `value` is a power of two.
    pub const fn new(value: u64) -> Option<Self> {
        if value.is_power_of_two() {
            Some(Self(value))
        } else {
            None
        }
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for Alignment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Alignment({:#x})", self.0)
    }
}

/// The page size of the guest architecture.
///
/// It is not a host page size:
///
/// ```compile_fail,E0308
/// use vmm::align::{GuestPageSize, HostPageSize};
/// fn guest(_: GuestPageSize) {}
/// guest(HostPageSize::new(4096).unwrap());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GuestPageSize(Alignment);

impl GuestPageSize {
    /// The x86 architectural base page.
    pub const X86_4K: Self = Self(Alignment(4096));

    pub const fn alignment(self) -> Alignment {
        self.0
    }
}

/// The host's base page size, as the host reports it.
///
/// It is not a guest page size:
///
/// ```compile_fail,E0308
/// use vmm::align::{GuestPageSize, HostPageSize};
/// fn host(_: HostPageSize) {}
/// host(GuestPageSize::X86_4K);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HostPageSize(Alignment);

impl HostPageSize {
    /// `None` unless `bytes` is a power of two.
    pub const fn new(bytes: u64) -> Option<Self> {
        match Alignment::new(bytes) {
            Some(alignment) => Some(Self(alignment)),
            None => None,
        }
    }

    pub const fn alignment(self) -> Alignment {
        self.0
    }
}

/// The page size of the host memory that backs guest memory: the granule of
/// its mappings and memslots.
///
/// It is not a host base page, which it may only exceed:
///
/// ```compile_fail,E0308
/// use vmm::align::{BackingPageSize, HostPageSize};
/// fn backing(_: BackingPageSize) {}
/// backing(HostPageSize::new(4096).unwrap());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BackingPageSize(Alignment);

impl BackingPageSize {
    /// The host's base pages, which back anonymous memory and plain memfds.
    pub const fn base(host: HostPageSize) -> Self {
        Self(host.0)
    }

    pub const fn alignment(self) -> Alignment {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alignments_are_powers_of_two() {
        for value in [0, 3, 0x1800, 4097, u64::MAX] {
            assert_eq!(Alignment::new(value), None, "{value:#x}");
        }
        for value in [1, 2, 0x1000, 1 << 30, 1 << 63] {
            assert_eq!(Alignment::new(value).map(Alignment::get), Some(value));
        }
        const GIB: Alignment = Alignment::new(1 << 30).unwrap();
        assert_eq!(format!("{GIB:?}"), "Alignment(0x40000000)");
    }

    #[test]
    fn page_sizes_are_powers_of_two() {
        assert_eq!(HostPageSize::new(0), None);
        assert_eq!(HostPageSize::new(4097), None);
        assert_eq!(
            HostPageSize::new(4096).map(|page| page.alignment().get()),
            Some(4096)
        );
        assert_eq!(
            HostPageSize::new(1 << 30).map(|page| page.alignment().get()),
            Some(1 << 30)
        );
        assert_eq!(GuestPageSize::X86_4K.alignment().get(), 4096);
        let host = HostPageSize::new(1 << 16).unwrap();
        assert_eq!(BackingPageSize::base(host).alignment(), host.alignment());
    }
}
