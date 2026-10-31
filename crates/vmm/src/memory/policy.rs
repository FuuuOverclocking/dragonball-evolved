//! How the host materializes a memory layout.
//!
//! The policy is not guest ABI: a snapshot keeps the layout, and a restore may
//! replace the policy.

use api::machine::MemoryBacking;

use crate::align::BackingPageSize;
use crate::kvm_context::HostCapabilities;
use crate::memory::layout::{MemoryLayout, Window, WindowId};
use crate::space::Gpa;

/// What holds guest memory on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackingKind {
    /// Private anonymous memory, mapped once per window.
    AnonymousPrivate,
    /// One sealed memfd per object, which other processes can map too.
    SharedMemfd,
}

impl From<MemoryBacking> for BackingKind {
    fn from(backing: MemoryBacking) -> Self {
        match backing {
            MemoryBacking::Anonymous => Self::AnonymousPrivate,
            MemoryBacking::Shared => Self::SharedMemfd,
        }
    }
}

/// The host side of guest memory, applied to every object of a layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostPolicy {
    pub kind: BackingKind,
    pub page_size: BackingPageSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum HostPolicyError {
    /// {count} boot windows need more than the {max} memslots KVM provides
    Memslots { count: usize, max: usize },
    /// window {id} ({window}) is not aligned to the {page:#x}-byte host page
    HostUnaligned {
        id: WindowId,
        window: Window,
        page: u64,
    },
    /// window {id} ({window}) is not aligned to the {page:#x}-byte backing page
    BackingUnaligned {
        id: WindowId,
        window: Window,
        page: u64,
    },
    /// window {id} ({window}) lies above {max}, the highest guest-physical address
    /// the host supports
    BeyondMaxGpa {
        id: WindowId,
        window: Window,
        max: Gpa,
    },
}

impl HostPolicy {
    /// Checks that `layout` can be materialized under this policy and bound on
    /// `host`: a memslot's address, size and host mapping are whole host pages,
    /// and a mapping is made of whole backing pages.
    pub fn check(
        &self,
        layout: &MemoryLayout,
        host: &HostCapabilities,
    ) -> Result<(), HostPolicyError> {
        let count = layout.boot_windows().count();
        if count > host.max_memslots {
            return Err(HostPolicyError::Memslots {
                count,
                max: host.max_memslots,
            });
        }
        let host_page = host.page_size.alignment();
        let backing_page = self.page_size.alignment();
        let max = host.max_gpa();
        for (id, &window) in layout.windows() {
            if !window.slice().is_aligned(host_page) || !window.placement().is_aligned(host_page) {
                return Err(HostPolicyError::HostUnaligned {
                    id,
                    window,
                    page: host_page.get(),
                });
            }
            if !window.slice().is_aligned(backing_page)
                || !window.placement().is_aligned(backing_page)
            {
                return Err(HostPolicyError::BackingUnaligned {
                    id,
                    window,
                    page: backing_page.get(),
                });
            }
            if window.placement().last().is_some_and(|last| last > max) {
                return Err(HostPolicyError::BeyondMaxGpa { id, window, max });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::layout::{Object, ObjectId, WindowKind, WindowOwner};
    use crate::space::{ObjectOffset, Range};

    const KINDS: [BackingKind; 2] = [BackingKind::AnonymousPrivate, BackingKind::SharedMemfd];

    fn base(kind: BackingKind, host: &HostCapabilities) -> HostPolicy {
        HostPolicy {
            kind,
            page_size: BackingPageSize::base(host.page_size),
        }
    }

    fn check(size: u64, host: &HostCapabilities) -> Result<(), HostPolicyError> {
        base(BackingKind::AnonymousPrivate, host).check(&MemoryLayout::boot(size).unwrap(), host)
    }

    /// One window of `len` bytes at `start`, from the start of its object.
    fn single(start: u64, len: u64) -> (MemoryLayout, Window) {
        let window = Window::new(
            ObjectId(0),
            Range::new(ObjectOffset(0), len).unwrap(),
            Gpa(start),
            WindowOwner::Boot,
            WindowKind::BootRam,
        )
        .unwrap();
        let layout = MemoryLayout::new(
            [(ObjectId(0), Object { capacity: len })],
            [(WindowId(0), window)],
        )
        .unwrap();
        (layout, window)
    }

    #[test]
    fn boot_layouts_fit_an_ordinary_host() {
        let host = HostCapabilities::fake(46, 32764, 4096);
        for size in [16 << 20, 4 << 30, 64 << 30] {
            let layout = MemoryLayout::boot(size).unwrap();
            for kind in KINDS {
                assert_eq!(base(kind, &host).check(&layout, &host), Ok(()));
            }
        }
    }

    #[test]
    fn boot_windows_must_fit_the_memslots_kvm_provides() {
        assert_eq!(
            check(4 << 30, &HostCapabilities::fake(46, 1, 4096)),
            Err(HostPolicyError::Memslots { count: 2, max: 1 })
        );
        assert_eq!(check(3 << 30, &HostCapabilities::fake(46, 1, 4096)), Ok(()));
        assert_eq!(check(4 << 30, &HostCapabilities::fake(46, 2, 4096)), Ok(()));
        assert_eq!(
            check(4 << 30, &HostCapabilities::fake(46, 0, 4096))
                .unwrap_err()
                .to_string(),
            "2 boot windows need more than the 0 memslots KVM provides"
        );
    }

    #[test]
    fn windows_must_lie_within_the_supported_physical_address_width() {
        let narrow = HostCapabilities::fake(36, 8, 4096);
        // Above the hole, 63 GiB of RAM ends exactly at the 64 GiB limit.
        assert_eq!(check(62 << 30, &narrow), Ok(()));
        assert_eq!(check(63 << 30, &narrow), Ok(()));
        let layout = MemoryLayout::boot((63 << 30) + 0x1000).unwrap();
        assert_eq!(
            base(BackingKind::SharedMemfd, &narrow).check(&layout, &narrow),
            Err(HostPolicyError::BeyondMaxGpa {
                id: WindowId(1),
                window: *layout.window(WindowId(1)).unwrap(),
                max: Gpa((1 << 36) - 1),
            })
        );
        assert!(matches!(
            check(64 << 30, &narrow),
            Err(HostPolicyError::BeyondMaxGpa { .. })
        ));
        assert_eq!(
            check(128 << 20, &HostCapabilities::fake(32, 8, 4096)),
            Ok(())
        );
        assert!(
            check(4 << 30, &HostCapabilities::fake(32, 8, 4096))
                .unwrap_err()
                .to_string()
                .contains("highest guest-physical address")
        );
    }

    #[test]
    fn windows_must_be_whole_host_and_backing_pages() {
        let (small, window) = single(0x1000, 0x1000);
        let (large, _) = single(0x1_0000, 0x1_0000);
        let large_pages = HostCapabilities::fake(46, 8, 0x1_0000);
        let policy = base(BackingKind::SharedMemfd, &large_pages);
        assert_eq!(
            policy.check(&small, &large_pages),
            Err(HostPolicyError::HostUnaligned {
                id: WindowId(0),
                window,
                page: 0x1_0000,
            })
        );
        assert_eq!(policy.check(&large, &large_pages), Ok(()));

        // A policy chosen for another host's pages, on a host with smaller ones.
        let small_pages = HostCapabilities::fake(46, 8, 0x1000);
        assert_eq!(
            policy.check(&small, &small_pages),
            Err(HostPolicyError::BackingUnaligned {
                id: WindowId(0),
                window,
                page: 0x1_0000,
            })
        );
        assert_eq!(policy.check(&large, &small_pages), Ok(()));
        assert_eq!(
            policy.check(&small, &small_pages).unwrap_err().to_string(),
            "window 0 (0x0..=0xfff of object 0 at 0x1000..=0x1fff) is not aligned to the \
             0x10000-byte backing page"
        );
    }

    #[test]
    fn api_backings_map_to_backing_kinds() {
        assert_eq!(
            BackingKind::from(MemoryBacking::Anonymous),
            BackingKind::AnonymousPrivate
        );
        assert_eq!(
            BackingKind::from(MemoryBacking::Shared),
            BackingKind::SharedMemfd
        );
    }
}
