//! Guest-ABI resources as values.
//!
//! A pool never changes in place: allocating returns the grant together with the
//! next pool, so a plan or transaction that fails just drops the new value. A grant
//! is plain data; `reserve` replays a recorded grant instead of allocating it again.

use std::collections::BTreeSet;
use std::iter;

use crate::align::Alignment;
use crate::space::{Range, Raw, Space};

#[derive(Debug, Clone, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum AddressPoolError<A: Space> {
    /// {pool} pool exhausted
    Exhausted { pool: &'static str },
    /// {pool} pool cannot allocate an empty range
    InvalidRequest { pool: &'static str },
    /// {range} is not inside the {pool} window {window}
    OutsideWindow {
        pool: &'static str,
        range: Range<A>,
        window: Range<A>,
    },
    /// {range} overlaps {allocated} in the {pool} pool
    Overlap {
        pool: &'static str,
        range: Range<A>,
        allocated: Range<A>,
    },
    /// {range} is not allocated in the {pool} pool
    NotAllocated { pool: &'static str, range: Range<A> },
}

/// Ranges handed out from a fixed window of a space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressPool<A: Space> {
    name: &'static str,
    window: Range<A>,
    /// Sorted by start and pairwise disjoint, so grants replayed in any order
    /// yield an equal pool.
    allocated: Vec<Range<A>>,
}

impl<A: Space> AddressPool<A> {
    pub fn new(name: &'static str, window: Range<A>) -> Self {
        Self {
            name,
            window,
            allocated: Vec::new(),
        }
    }

    /// Grants the lowest free `len` points that start at a multiple of `align`.
    pub fn allocate(
        &self,
        len: A::Raw,
        align: Alignment,
    ) -> Result<(Range<A>, Self), AddressPoolError<A>> {
        if len == A::Raw::ZERO {
            return Err(AddressPoolError::InvalidRequest { pool: self.name });
        }
        let range = self
            .gaps()
            .find_map(|gap| {
                let range = Range::new(gap.start().align_up(align)?, len)?;
                gap.contains_range(range).then_some(range)
            })
            .ok_or(AddressPoolError::Exhausted { pool: self.name })?;
        Ok((range, self.with(range)))
    }

    /// Records an earlier grant again, as when restoring. The range must be a
    /// non-empty part of the window that overlaps no allocation.
    pub fn reserve(&self, range: Range<A>) -> Result<Self, AddressPoolError<A>> {
        if range.is_empty() || !self.window.contains_range(range) {
            return Err(AddressPoolError::OutsideWindow {
                pool: self.name,
                range,
                window: self.window,
            });
        }
        if let Some(&allocated) = self.allocated.iter().find(|other| other.overlaps(range)) {
            return Err(AddressPoolError::Overlap {
                pool: self.name,
                range,
                allocated,
            });
        }
        Ok(self.with(range))
    }

    /// Returns a grant to the pool. Only a whole grant can be released.
    pub fn release(&self, range: Range<A>) -> Result<Self, AddressPoolError<A>> {
        let index = self
            .allocated
            .iter()
            .position(|allocated| *allocated == range)
            .ok_or(AddressPoolError::NotAllocated {
                pool: self.name,
                range,
            })?;
        let mut next = self.clone();
        next.allocated.remove(index);
        Ok(next)
    }

    /// The free parts of the window, lowest first: before, between and after
    /// the allocations.
    fn gaps(&self) -> impl Iterator<Item = Range<A>> {
        let froms =
            iter::once(Some(self.window.start())).chain(self.allocated.iter().map(|r| r.end()));
        let untils = self.allocated.iter().map(|r| Some(r.start())).chain([None]);
        froms.zip(untils).filter_map(|(from, until)| {
            // An allocation that ends at the top of the space leaves no gap after it.
            let rest = self.window.split_at(from?)?.1;
            match until {
                Some(until) => Some(rest.split_at(until)?.0),
                None => Some(rest),
            }
        })
    }

    fn with(&self, range: Range<A>) -> Self {
        let mut next = self.clone();
        let index = next
            .allocated
            .partition_point(|other| other.start() < range.start());
        next.allocated.insert(index, range);
        next
    }
}

#[derive(Debug, Clone, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum IdPoolError<T: Space> {
    /// {pool} pool exhausted
    Exhausted { pool: &'static str },
    /// {id} is outside the {pool} pool
    OutOfRange { pool: &'static str, id: T },
    /// {id} is already allocated in the {pool} pool
    InUse { pool: &'static str, id: T },
    /// {id} is not allocated in the {pool} pool
    NotAllocated { pool: &'static str, id: T },
}

/// Ids, such as interrupt lines, handed out from a fixed range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdPool<T: Space> {
    name: &'static str,
    ids: Range<T>,
    allocated: BTreeSet<T>,
}

impl<T: Space> IdPool<T> {
    pub fn new(name: &'static str, ids: Range<T>) -> Self {
        Self {
            name,
            ids,
            allocated: BTreeSet::new(),
        }
    }

    /// Grants the lowest free id.
    pub fn allocate(&self) -> Result<(T, Self), IdPoolError<T>> {
        // Grants lie within `ids`, so the lowest free id ends the run of grants
        // from its start.
        let mut free = Some(self.ids.start());
        for &id in &self.allocated {
            if Some(id) != free {
                break;
            }
            free = id.raw().checked_add(T::Raw::ONE).map(T::from_raw);
        }
        let id = free
            .filter(|&id| self.ids.contains(id))
            .ok_or(IdPoolError::Exhausted { pool: self.name })?;
        let mut next = self.clone();
        next.allocated.insert(id);
        Ok((id, next))
    }

    /// Records an earlier grant again, as when restoring.
    pub fn reserve(&self, id: T) -> Result<Self, IdPoolError<T>> {
        if !self.ids.contains(id) {
            return Err(IdPoolError::OutOfRange {
                pool: self.name,
                id,
            });
        }
        let mut next = self.clone();
        if !next.allocated.insert(id) {
            return Err(IdPoolError::InUse {
                pool: self.name,
                id,
            });
        }
        Ok(next)
    }

    /// Returns a grant to the pool.
    pub fn release(&self, id: T) -> Result<Self, IdPoolError<T>> {
        let mut next = self.clone();
        if !next.allocated.remove(&id) {
            return Err(IdPoolError::NotAllocated {
                pool: self.name,
                id,
            });
        }
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use std::ops;

    use super::*;
    use crate::space::{Gpa, Gsi, Pio};

    const WINDOW: Range<Gpa> = Range::<Gpa>::const_new(Gpa(0x1000), 0x3000).unwrap();
    const MEMORY_WINDOW: Range<Gpa> = Range::<Gpa>::const_new(Gpa(4 << 30), 2 << 30).unwrap();
    const GSIS: Range<Gsi> = Range::<Gsi>::const_inclusive(Gsi(5), Gsi(7)).unwrap();

    /// The pools a machine plan keeps together.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Book {
        virtio_mmio: AddressPool<Gpa>,
        legacy_gsi: IdPool<Gsi>,
        device_memory: AddressPool<Gpa>,
    }

    fn fresh_book() -> Book {
        Book {
            virtio_mmio: AddressPool::new("test MMIO", WINDOW),
            legacy_gsi: IdPool::new("test GSI", GSIS),
            device_memory: AddressPool::new("device memory", MEMORY_WINDOW),
        }
    }

    fn gpa(range: ops::Range<u64>) -> Range<Gpa> {
        range.try_into().unwrap()
    }

    fn align(value: u64) -> Alignment {
        Alignment::new(value).unwrap()
    }

    #[test]
    fn addresses_are_granted_lowest_first_and_aligned() {
        let pool = AddressPool::new("test", gpa(0x1800..0x8000));
        let (first, pool) = pool.allocate(0x100, align(0x1000)).unwrap();
        assert_eq!(first, gpa(0x2000..0x2100));
        let (second, pool) = pool.allocate(0x1000, align(0x1000)).unwrap();
        assert_eq!(second, gpa(0x3000..0x4000));
        // Smaller requests fill the gaps below and between earlier grants.
        let (third, pool) = pool.allocate(0x800, align(0x800)).unwrap();
        assert_eq!(third, gpa(0x1800..0x2000));
        let (fourth, pool) = pool.allocate(0x10, align(0x10)).unwrap();
        assert_eq!(fourth, gpa(0x2100..0x2110));
        let pool = pool.release(second).unwrap();
        assert_eq!(pool.allocate(0x1000, align(0x1000)).unwrap().0, second);
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
        let error = pool.allocate(0, align(0x1000)).unwrap_err();
        assert_eq!(error, AddressPoolError::InvalidRequest { pool: "test" });
        assert_eq!(
            error.to_string(),
            "test pool cannot allocate an empty range"
        );
        // Alignments that are not powers of two cannot be requested at all.
        for value in [0, 0x1800] {
            assert_eq!(Alignment::new(value), None);
        }
    }

    #[test]
    fn exhaustion_names_the_pool() {
        let (_, pool) = AddressPool::new("test MMIO", WINDOW)
            .allocate(0x2000, align(0x1000))
            .unwrap();
        let error = pool.allocate(0x2000, align(0x1000)).unwrap_err();
        assert_eq!(error, AddressPoolError::Exhausted { pool: "test MMIO" });
        assert_eq!(error.to_string(), "test MMIO pool exhausted");
        // Requests that would run past the top of the address space do not fit either.
        let top = AddressPool::new("test MMIO", gpa(u64::MAX - 0xfff..u64::MAX));
        for (len, alignment) in [(0x2000, 1), (0x1000, 1 << 63)] {
            assert_eq!(top.allocate(len, align(alignment)).unwrap_err(), error);
        }

        let mut pool = IdPool::new("test GSI", GSIS);
        for _ in 0..3 {
            pool = pool.allocate().unwrap().1;
        }
        let error = pool.allocate().unwrap_err();
        assert_eq!(error, IdPoolError::Exhausted { pool: "test GSI" });
        assert_eq!(error.to_string(), "test GSI pool exhausted");
    }

    #[test]
    fn reserve_rejects_ranges_outside_the_window_or_overlapping_a_grant() {
        let (granted, pool) = AddressPool::new("test", WINDOW)
            .allocate(0x1000, align(0x1000))
            .unwrap();
        for range in [gpa(0..0x1000), gpa(0x3800..0x4800), gpa(0x2000..0x2000)] {
            assert_eq!(
                pool.reserve(range).unwrap_err(),
                AddressPoolError::OutsideWindow {
                    pool: "test",
                    range,
                    window: WINDOW,
                }
            );
        }
        let range = gpa(0x1800..0x2800);
        let error = pool.reserve(range).unwrap_err();
        assert_eq!(
            error,
            AddressPoolError::Overlap {
                pool: "test",
                range,
                allocated: granted,
            }
        );
        assert_eq!(
            error.to_string(),
            "0x1800..=0x27ff overlaps 0x1000..=0x1fff in the test pool"
        );
        let pool = pool.reserve(gpa(0x2000..0x3000)).unwrap();
        assert_eq!(
            pool.allocate(0x1000, align(0x1000)).unwrap().0,
            gpa(0x3000..0x4000)
        );

        let (granted, pool) = IdPool::new("test", GSIS).allocate().unwrap();
        assert_eq!(
            pool.reserve(granted).unwrap_err(),
            IdPoolError::InUse {
                pool: "test",
                id: Gsi(5),
            }
        );
        for id in [4, 8] {
            let error = pool.reserve(Gsi(id)).unwrap_err();
            assert_eq!(
                error,
                IdPoolError::OutOfRange {
                    pool: "test",
                    id: Gsi(id),
                }
            );
            assert_eq!(error.to_string(), format!("{id} is outside the test pool"));
        }
    }

    #[test]
    fn release_accepts_only_a_whole_grant() {
        let pool = AddressPool::new("test", WINDOW);
        let (granted, allocated) = pool.allocate(0x2000, align(0x1000)).unwrap();
        for range in [gpa(0x1000..0x2000), gpa(0x3000..0x4000)] {
            assert_eq!(
                allocated.release(range).unwrap_err(),
                AddressPoolError::NotAllocated {
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
            IdPoolError::NotAllocated {
                pool: "test",
                id: Gsi(5),
            }
        );
        assert_eq!(allocated.release(granted).unwrap(), pool);
    }

    #[test]
    fn allocating_leaves_the_original_book_unchanged() {
        let book = fresh_book();
        let (_, virtio_mmio) = book.virtio_mmio.allocate(0x1000, align(0x1000)).unwrap();
        let (_, legacy_gsi) = book.legacy_gsi.allocate().unwrap();
        let (_, device_memory) = book
            .device_memory
            .allocate(1 << 30, align(1 << 30))
            .unwrap();
        let next = Book {
            virtio_mmio,
            legacy_gsi,
            device_memory,
        };
        assert_ne!(next, book);
        assert_eq!(book, fresh_book());
    }

    #[test]
    fn reserving_recorded_grants_replays_the_book() {
        let mut book = fresh_book();
        let mut grants = Vec::new();
        for len in [0x1000, 0x800, 0x1000] {
            let (mmio, virtio_mmio) = book.virtio_mmio.allocate(len, align(0x1000)).unwrap();
            let (gsi, legacy_gsi) = book.legacy_gsi.allocate().unwrap();
            let (memory, device_memory) = book
                .device_memory
                .allocate(1 << 29, align(1 << 29))
                .unwrap();
            book = Book {
                virtio_mmio,
                legacy_gsi,
                device_memory,
            };
            grants.push((mmio, gsi, memory));
        }
        // Replaying out of order still yields an equal book.
        let mut replayed = fresh_book();
        for &(mmio, gsi, memory) in grants.iter().rev() {
            replayed = Book {
                virtio_mmio: replayed.virtio_mmio.reserve(mmio).unwrap(),
                legacy_gsi: replayed.legacy_gsi.reserve(gsi).unwrap(),
                device_memory: replayed.device_memory.reserve(memory).unwrap(),
            };
        }
        assert_eq!(replayed, book);
    }

    #[test]
    fn device_memory_grants_are_bounded_and_disjoint() {
        let original = fresh_book().device_memory;
        let (first, pool) = original.allocate(1 << 30, align(1 << 30)).unwrap();
        assert_eq!(first, gpa(4 << 30..5 << 30));
        let (second, full) = pool.allocate(1 << 30, align(1 << 30)).unwrap();
        assert_eq!(second, gpa(5 << 30..6 << 30));
        assert_eq!(
            full.allocate(1 << 30, align(1 << 30)).unwrap_err(),
            AddressPoolError::Exhausted {
                pool: "device memory"
            }
        );
        let range = gpa(5 << 30..7 << 30);
        assert_eq!(
            original.reserve(range).unwrap_err(),
            AddressPoolError::OutsideWindow {
                pool: "device memory",
                range,
                window: MEMORY_WINDOW,
            }
        );
        assert_eq!(
            pool.reserve(first).unwrap_err(),
            AddressPoolError::Overlap {
                pool: "device memory",
                range: first,
                allocated: first,
            }
        );
        assert_eq!(
            full.release(first).unwrap().release(second).unwrap(),
            original
        );
    }

    #[test]
    fn pools_reach_the_top_of_their_space() {
        let window = Range::<Pio>::const_inclusive(Pio(0xfff0), Pio(0xffff)).unwrap();
        let pool = AddressPool::new("ports", window);
        let (low, pool) = pool.allocate(8, align(8)).unwrap();
        let (high, full) = pool.allocate(8, align(8)).unwrap();
        assert_eq!(low.to_string(), "0xfff0..=0xfff7");
        assert_eq!(high.to_string(), "0xfff8..=0xffff");
        assert_eq!(
            full.allocate(1, align(1)).unwrap_err(),
            AddressPoolError::Exhausted { pool: "ports" }
        );
        // The gap below a grant that ends at the top is still found.
        let pool = AddressPool::new("ports", window).reserve(high).unwrap();
        assert_eq!(pool.allocate(8, align(1)).unwrap().0, low);
        assert_eq!(
            pool.reserve(Range::new(Pio(0xffff), 1).unwrap())
                .unwrap_err(),
            AddressPoolError::Overlap {
                pool: "ports",
                range: Range::new(Pio(0xffff), 1).unwrap(),
                allocated: high,
            }
        );

        let top = Range::<Gpa>::const_new(Gpa(u64::MAX - 0xfff), 0x1000).unwrap();
        let (granted, _) = AddressPool::new("top", top)
            .allocate(0x1000, align(0x1000))
            .unwrap();
        assert_eq!(granted, top);

        let ids = Range::<Gsi>::const_inclusive(Gsi(u32::MAX - 1), Gsi(u32::MAX)).unwrap();
        let (first, pool) = IdPool::new("top", ids).allocate().unwrap();
        let (second, full) = pool.allocate().unwrap();
        assert_eq!((first, second), (Gsi(u32::MAX - 1), Gsi(u32::MAX)));
        assert_eq!(
            full.allocate().unwrap_err(),
            IdPoolError::Exhausted { pool: "top" }
        );
        assert_eq!(full.release(first).unwrap().allocate().unwrap().0, first);
    }

    #[test]
    fn empty_id_ranges_grant_nothing() {
        let pool = IdPool::new("none", Range::new(Gsi(5), 0).unwrap());
        assert_eq!(
            pool.allocate().unwrap_err(),
            IdPoolError::Exhausted { pool: "none" }
        );
        assert_eq!(
            pool.reserve(Gsi(5)).unwrap_err(),
            IdPoolError::OutOfRange {
                pool: "none",
                id: Gsi(5),
            }
        );
    }
}
