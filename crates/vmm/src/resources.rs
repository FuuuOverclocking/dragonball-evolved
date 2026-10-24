//! Guest-ABI resources as values.
//!
//! A pool never changes in place: allocating returns the grant together with the
//! next pool, so a plan or transaction that fails just drops the new value. A grant
//! is plain data; `reserve` replays a recorded grant instead of allocating it again.

use std::collections::BTreeSet;
use std::range::{Range, RangeInclusive};
use std::{fmt, iter};

use thiserror::Error;

/// A global system interrupt: an input line of the guest's interrupt controllers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Gsi(pub u32);

impl From<u32> for Gsi {
    fn from(line: u32) -> Self {
        Self(line)
    }
}

impl From<Gsi> for u32 {
    fn from(gsi: Gsi) -> Self {
        gsi.0
    }
}

/// A half-open range of guest-physical addresses that never ends before it starts.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GpaRange(Range<u64>);

impl GpaRange {
    /// # Panics
    ///
    /// Panics if `end < start`, which fails the build when evaluated in a constant.
    pub const fn new(start: u64, end: u64) -> Self {
        assert!(start <= end, "GPA range ends before it starts");
        Self(Range { start, end })
    }

    pub const fn start(&self) -> u64 {
        self.0.start
    }

    pub const fn end(&self) -> u64 {
        self.0.end
    }

    pub const fn len(&self) -> u64 {
        self.0.end - self.0.start
    }

    pub const fn is_empty(&self) -> bool {
        self.0.start == self.0.end
    }

    /// Whether the two ranges share an address.
    pub fn overlaps(&self, other: &Self) -> bool {
        self.start().max(other.start()) < self.end().min(other.end())
    }
}

impl From<GpaRange> for Range<u64> {
    fn from(range: GpaRange) -> Self {
        range.0
    }
}

impl fmt::Debug for GpaRange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "GpaRange({self})")
    }
}

impl fmt::Display for GpaRange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:#x}..{:#x}", self.start(), self.end())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ResourceError {
    #[error("{pool} pool exhausted")]
    Exhausted { pool: &'static str },
    #[error("{pool} pool cannot allocate {len:#x} bytes aligned to {align:#x}")]
    InvalidRequest {
        pool: &'static str,
        len: u64,
        align: u64,
    },
    #[error("{range} is not inside the {pool} window {window}")]
    OutsideWindow {
        pool: &'static str,
        range: GpaRange,
        window: GpaRange,
    },
    #[error("{range} overlaps {allocated} in the {pool} pool")]
    Overlap {
        pool: &'static str,
        range: GpaRange,
        allocated: GpaRange,
    },
    #[error("{range} is not allocated in the {pool} pool")]
    RangeNotAllocated { pool: &'static str, range: GpaRange },
    #[error("{id} is outside the {pool} pool")]
    IdOutOfRange { pool: &'static str, id: u32 },
    #[error("{id} is already allocated in the {pool} pool")]
    IdInUse { pool: &'static str, id: u32 },
    #[error("{id} is not allocated in the {pool} pool")]
    IdNotAllocated { pool: &'static str, id: u32 },
}

/// Guest-physical address ranges handed out from a fixed window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressPool {
    name: &'static str,
    window: GpaRange,
    /// Sorted by address and pairwise disjoint, so grants replayed in any order
    /// yield an equal pool.
    allocated: Vec<GpaRange>,
}

impl AddressPool {
    pub fn new(name: &'static str, window: GpaRange) -> Self {
        Self {
            name,
            window,
            allocated: Vec::new(),
        }
    }

    /// Grants the lowest free `len` bytes that start at a multiple of `align`.
    pub fn allocate(&self, len: u64, align: u64) -> Result<(GpaRange, Self), ResourceError> {
        if len == 0 || !align.is_power_of_two() {
            return Err(ResourceError::InvalidRequest {
                pool: self.name,
                len,
                align,
            });
        }
        // The free gaps lie before, between and after the allocations.
        let gap_starts =
            iter::once(self.window.start()).chain(self.allocated.iter().map(GpaRange::end));
        let gap_ends = self
            .allocated
            .iter()
            .map(GpaRange::start)
            .chain([self.window.end()]);
        let range = gap_starts
            .zip(gap_ends)
            .find_map(|(gap_start, gap_end)| {
                let start = gap_start.checked_next_multiple_of(align)?;
                let end = start.checked_add(len)?;
                (end <= gap_end).then_some(GpaRange::new(start, end))
            })
            .ok_or(ResourceError::Exhausted { pool: self.name })?;
        Ok((range, self.with(range)))
    }

    /// Records an earlier grant again, as when restoring. The range must be a
    /// non-empty part of the window that overlaps no allocation.
    pub fn reserve(&self, range: GpaRange) -> Result<Self, ResourceError> {
        if range.is_empty()
            || range.start() < self.window.start()
            || range.end() > self.window.end()
        {
            return Err(ResourceError::OutsideWindow {
                pool: self.name,
                range,
                window: self.window,
            });
        }
        if let Some(&allocated) = self.allocated.iter().find(|other| other.overlaps(&range)) {
            return Err(ResourceError::Overlap {
                pool: self.name,
                range,
                allocated,
            });
        }
        Ok(self.with(range))
    }

    /// Returns a grant to the pool. Only a whole grant can be released.
    pub fn release(&self, range: GpaRange) -> Result<Self, ResourceError> {
        let index = self
            .allocated
            .iter()
            .position(|allocated| *allocated == range)
            .ok_or(ResourceError::RangeNotAllocated {
                pool: self.name,
                range,
            })?;
        let mut next = self.clone();
        next.allocated.remove(index);
        Ok(next)
    }

    fn with(&self, range: GpaRange) -> Self {
        let mut next = self.clone();
        let index = next
            .allocated
            .partition_point(|other| other.start() < range.start());
        next.allocated.insert(index, range);
        next
    }
}

/// Small integer ids, such as interrupt lines, handed out from a fixed range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdPool<T> {
    name: &'static str,
    ids: RangeInclusive<T>,
    allocated: BTreeSet<T>,
}

impl<T> IdPool<T>
where
    T: Copy + Ord + From<u32> + Into<u32>,
{
    pub fn new(name: &'static str, ids: RangeInclusive<T>) -> Self {
        Self {
            name,
            ids,
            allocated: BTreeSet::new(),
        }
    }

    /// Grants the lowest free id.
    pub fn allocate(&self) -> Result<(T, Self), ResourceError> {
        // Ids are walked as u32 because user types cannot implement `Step`.
        let span: RangeInclusive<u32> = RangeInclusive {
            start: self.ids.start.into(),
            last: self.ids.last.into(),
        };
        let id = span
            .iter()
            .map(T::from)
            .find(|id| !self.allocated.contains(id))
            .ok_or(ResourceError::Exhausted { pool: self.name })?;
        let mut next = self.clone();
        next.allocated.insert(id);
        Ok((id, next))
    }

    /// Records an earlier grant again, as when restoring.
    pub fn reserve(&self, id: T) -> Result<Self, ResourceError> {
        if !self.ids.contains(&id) {
            return Err(ResourceError::IdOutOfRange {
                pool: self.name,
                id: id.into(),
            });
        }
        let mut next = self.clone();
        if !next.allocated.insert(id) {
            return Err(ResourceError::IdInUse {
                pool: self.name,
                id: id.into(),
            });
        }
        Ok(next)
    }

    /// Returns a grant to the pool.
    pub fn release(&self, id: T) -> Result<Self, ResourceError> {
        let mut next = self.clone();
        if !next.allocated.remove(&id) {
            return Err(ResourceError::IdNotAllocated {
                pool: self.name,
                id: id.into(),
            });
        }
        Ok(next)
    }
}

/// The guest-ABI resources a machine hands out to its devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceBook {
    pub virtio_mmio: AddressPool,
    pub legacy_gsi: IdPool<Gsi>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: GpaRange = GpaRange::new(0x1000, 0x4000);
    const GSIS: RangeInclusive<Gsi> = RangeInclusive {
        start: Gsi(5),
        last: Gsi(7),
    };

    fn fresh_book() -> ResourceBook {
        ResourceBook {
            virtio_mmio: AddressPool::new("test MMIO", WINDOW),
            legacy_gsi: IdPool::new("test GSI", GSIS),
        }
    }

    #[test]
    fn ranges_overlap_only_when_they_share_an_address() {
        let range = GpaRange::new(0x1000, 0x2000);
        assert!(range.overlaps(&GpaRange::new(0x1fff, 0x3000)));
        assert!(range.overlaps(&GpaRange::new(0, 0x4000)));
        assert!(!range.overlaps(&GpaRange::new(0x2000, 0x3000)));
        assert!(!range.overlaps(&GpaRange::new(0x1800, 0x1800)));
    }

    #[test]
    fn addresses_are_granted_lowest_first_and_aligned() {
        let pool = AddressPool::new("test", GpaRange::new(0x1800, 0x8000));
        let (first, pool) = pool.allocate(0x100, 0x1000).unwrap();
        assert_eq!(first, GpaRange::new(0x2000, 0x2100));
        let (second, pool) = pool.allocate(0x1000, 0x1000).unwrap();
        assert_eq!(second, GpaRange::new(0x3000, 0x4000));
        // Smaller requests fill the gaps below and between earlier grants.
        let (third, pool) = pool.allocate(0x800, 0x800).unwrap();
        assert_eq!(third, GpaRange::new(0x1800, 0x2000));
        let (fourth, pool) = pool.allocate(0x10, 0x10).unwrap();
        assert_eq!(fourth, GpaRange::new(0x2100, 0x2110));
        let pool = pool.release(second).unwrap();
        assert_eq!(pool.allocate(0x1000, 0x1000).unwrap().0, second);
    }

    #[test]
    fn ids_are_granted_lowest_first() {
        let pool = IdPool::new("test", GSIS);
        let (first, pool) = pool.allocate().unwrap();
        let (second, pool) = pool.allocate().unwrap();
        assert_eq!((first, second), (Gsi(5), Gsi(6)));
        let pool = pool.release(first).unwrap();
        assert_eq!(pool.allocate().unwrap().0, Gsi(5));
    }

    #[test]
    fn malformed_address_requests_are_rejected() {
        let pool = AddressPool::new("test", WINDOW);
        for (len, align) in [(0, 0x1000), (0x1000, 0), (0x1000, 0x1800)] {
            assert_eq!(
                pool.allocate(len, align).unwrap_err(),
                ResourceError::InvalidRequest {
                    pool: "test",
                    len,
                    align,
                }
            );
        }
    }

    #[test]
    fn exhaustion_names_the_pool() {
        let (_, pool) = AddressPool::new("test MMIO", WINDOW)
            .allocate(0x2000, 0x1000)
            .unwrap();
        let error = pool.allocate(0x2000, 0x1000).unwrap_err();
        assert_eq!(error, ResourceError::Exhausted { pool: "test MMIO" });
        assert_eq!(error.to_string(), "test MMIO pool exhausted");
        // Requests that would run past the top of the address space do not fit either.
        let top = AddressPool::new("test MMIO", GpaRange::new(u64::MAX - 0xfff, u64::MAX));
        for (len, align) in [(0x2000, 1), (0x1000, 1 << 63)] {
            assert_eq!(top.allocate(len, align).unwrap_err(), error);
        }

        let mut pool = IdPool::new("test GSI", GSIS);
        for _ in 0..3 {
            pool = pool.allocate().unwrap().1;
        }
        let error = pool.allocate().unwrap_err();
        assert_eq!(error, ResourceError::Exhausted { pool: "test GSI" });
        assert_eq!(error.to_string(), "test GSI pool exhausted");
    }

    #[test]
    fn reserve_rejects_ranges_outside_the_window_or_overlapping_a_grant() {
        let (granted, pool) = AddressPool::new("test", WINDOW)
            .allocate(0x1000, 0x1000)
            .unwrap();
        for range in [
            GpaRange::new(0, 0x1000),
            GpaRange::new(0x3800, 0x4800),
            GpaRange::new(0x2000, 0x2000),
        ] {
            assert_eq!(
                pool.reserve(range).unwrap_err(),
                ResourceError::OutsideWindow {
                    pool: "test",
                    range,
                    window: WINDOW,
                }
            );
        }
        let range = GpaRange::new(0x1800, 0x2800);
        assert_eq!(
            pool.reserve(range).unwrap_err(),
            ResourceError::Overlap {
                pool: "test",
                range,
                allocated: granted,
            }
        );
        let pool = pool.reserve(GpaRange::new(0x2000, 0x3000)).unwrap();
        assert_eq!(
            pool.allocate(0x1000, 0x1000).unwrap().0,
            GpaRange::new(0x3000, 0x4000)
        );

        let (granted, pool) = IdPool::new("test", GSIS).allocate().unwrap();
        assert_eq!(
            pool.reserve(granted).unwrap_err(),
            ResourceError::IdInUse {
                pool: "test",
                id: 5,
            }
        );
        for id in [4, 8] {
            assert_eq!(
                pool.reserve(Gsi(id)).unwrap_err(),
                ResourceError::IdOutOfRange { pool: "test", id }
            );
        }
    }

    #[test]
    fn release_accepts_only_a_whole_grant() {
        let pool = AddressPool::new("test", WINDOW);
        let (granted, allocated) = pool.allocate(0x2000, 0x1000).unwrap();
        for range in [GpaRange::new(0x1000, 0x2000), GpaRange::new(0x3000, 0x4000)] {
            assert_eq!(
                allocated.release(range).unwrap_err(),
                ResourceError::RangeNotAllocated {
                    pool: "test",
                    range,
                }
            );
        }
        assert_eq!(allocated.release(granted).unwrap(), pool);

        let pool = IdPool::new("test", GSIS);
        let (granted, allocated) = pool.allocate().unwrap();
        assert_eq!(
            pool.release(granted).unwrap_err(),
            ResourceError::IdNotAllocated {
                pool: "test",
                id: 5,
            }
        );
        assert_eq!(allocated.release(granted).unwrap(), pool);
    }

    #[test]
    fn allocating_leaves_the_original_book_unchanged() {
        let book = fresh_book();
        let (_, virtio_mmio) = book.virtio_mmio.allocate(0x1000, 0x1000).unwrap();
        let (_, legacy_gsi) = book.legacy_gsi.allocate().unwrap();
        let next = ResourceBook {
            virtio_mmio,
            legacy_gsi,
        };
        assert_ne!(next, book);
        assert_eq!(book, fresh_book());
    }

    #[test]
    fn reserving_recorded_grants_replays_the_book() {
        let mut book = fresh_book();
        let mut grants = Vec::new();
        for len in [0x1000, 0x800, 0x1000] {
            let (mmio, virtio_mmio) = book.virtio_mmio.allocate(len, 0x1000).unwrap();
            let (gsi, legacy_gsi) = book.legacy_gsi.allocate().unwrap();
            book = ResourceBook {
                virtio_mmio,
                legacy_gsi,
            };
            grants.push((mmio, gsi));
        }
        // Replaying out of order still yields an equal book.
        let mut replayed = fresh_book();
        for &(mmio, gsi) in grants.iter().rev() {
            replayed = ResourceBook {
                virtio_mmio: replayed.virtio_mmio.reserve(mmio).unwrap(),
                legacy_gsi: replayed.legacy_gsi.reserve(gsi).unwrap(),
            };
        }
        assert_eq!(replayed, book);
    }
}
