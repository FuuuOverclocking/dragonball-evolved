//! Typed address and id spaces, and ranges of their points.
//!
//! Each space is its own point type, so ranges of different spaces do not mix:
//!
//! ```compile_fail,E0308
//! use vmm::space::{Gpa, Pio, Range};
//! let ports = Range::<Pio>::const_new(Pio(0x60), 5).unwrap();
//! let memory = Range::<Gpa>::const_new(Gpa(0x60), 5).unwrap();
//! ports.overlaps(memory);
//! ```
//!
//! An offset within a storage object is not a guest-physical address either:
//!
//! ```compile_fail,E0308
//! use vmm::space::{Gpa, ObjectOffset, Range};
//! let slice = Range::<ObjectOffset>::const_new(ObjectOffset(0), 0x1000).unwrap();
//! let placement = Range::<Gpa>::const_new(Gpa(0), 0x1000).unwrap();
//! slice.overlaps(placement);
//! ```

use std::fmt;
use std::hash::Hash;
use std::ops::{self, Add, Sub};

use crate::align::Alignment;

mod sealed {
    pub trait Sealed {}
}

/// The integer type of a space's points and of its range lengths.
pub trait Raw:
    sealed::Sealed
    + Copy
    + Ord
    + Hash
    + fmt::Debug
    + Add<Output = Self>
    + Sub<Output = Self>
    + TryFrom<usize>
{
    const ZERO: Self;
    const ONE: Self;

    fn checked_add(self, rhs: Self) -> Option<Self>;
    fn checked_sub(self, rhs: Self) -> Option<Self>;
    fn is_aligned(self, align: Alignment) -> bool;
    fn align_down(self, align: Alignment) -> Self;
    /// `None` if the next aligned value is past the top of the type.
    fn align_up(self, align: Alignment) -> Option<Self>;
}

macro_rules! raw {
    ($($int:ty),*) => {$(
        impl sealed::Sealed for $int {}

        impl Raw for $int {
            const ZERO: Self = 0;
            const ONE: Self = 1;

            fn checked_add(self, rhs: Self) -> Option<Self> {
                <$int>::checked_add(self, rhs)
            }

            fn checked_sub(self, rhs: Self) -> Option<Self> {
                <$int>::checked_sub(self, rhs)
            }

            fn is_aligned(self, align: Alignment) -> bool {
                match Self::try_from(align.get() - 1) {
                    Ok(mask) => self & mask == 0,
                    Err(_) => self == 0,
                }
            }

            fn align_down(self, align: Alignment) -> Self {
                match Self::try_from(align.get() - 1) {
                    Ok(mask) => self & !mask,
                    Err(_) => 0,
                }
            }

            fn align_up(self, align: Alignment) -> Option<Self> {
                match Self::try_from(align.get() - 1) {
                    Ok(mask) => self.checked_add(mask).map(|value| value & !mask),
                    Err(_) => (self == 0).then_some(0),
                }
            }
        }
    )*};
}

raw!(u16, u32, u64);

/// A point of an address or id space; the type stands for its space.
pub trait Space: Copy + Ord + Hash + fmt::Debug + fmt::Display {
    type Raw: Raw;

    fn from_raw(raw: Self::Raw) -> Self;
    fn raw(self) -> Self::Raw;

    fn is_aligned(self, align: Alignment) -> bool {
        self.raw().is_aligned(align)
    }

    fn align_down(self, align: Alignment) -> Self {
        Self::from_raw(self.raw().align_down(align))
    }

    /// `None` if the next aligned point is past the top of the space.
    fn align_up(self, align: Alignment) -> Option<Self> {
        self.raw().align_up(align).map(Self::from_raw)
    }
}

/// A range of points of one space: `len` points from `start`.
///
/// A range may end at the top of its space, so [`Range::last`] is the primary
/// bound and [`Range::end`] is checked. Lengths have the space's own integer
/// type, so no range covers the whole space.
///
/// An empty range has a position but no points: it overlaps nothing, and it
/// lies within a range only between that range's start and end.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Range<A: Space> {
    start: A,
    len: A::Raw,
}

impl<A: Space> Range<A> {
    /// `None` if the range would run past the top of the space.
    pub fn new(start: A, len: A::Raw) -> Option<Self> {
        let fits = len == A::Raw::ZERO || start.raw().checked_add(len - A::Raw::ONE).is_some();
        fits.then_some(Self { start, len })
    }

    /// The points `start..=last`; `None` if `last < start`, or if they cover
    /// the whole space.
    pub fn inclusive(start: A, last: A) -> Option<Self> {
        let len = last
            .raw()
            .checked_sub(start.raw())?
            .checked_add(A::Raw::ONE)?;
        Some(Self { start, len })
    }

    pub const fn start(self) -> A {
        self.start
    }

    pub const fn len(self) -> A::Raw {
        self.len
    }

    pub fn is_empty(self) -> bool {
        self.len == A::Raw::ZERO
    }

    /// `None` if the range is empty.
    pub fn last(self) -> Option<A> {
        let offset = self.len.checked_sub(A::Raw::ONE)?;
        Some(A::from_raw(self.start.raw() + offset))
    }

    /// The point just past the range; `None` if the range ends at the top of
    /// the space.
    pub fn end(self) -> Option<A> {
        self.start.raw().checked_add(self.len).map(A::from_raw)
    }

    pub fn contains(self, point: A) -> bool {
        point >= self.start && point.raw() - self.start.raw() < self.len
    }

    pub fn contains_range(self, other: Self) -> bool {
        other
            .start
            .raw()
            .checked_sub(self.start.raw())
            .and_then(|offset| self.len.checked_sub(offset))
            .is_some_and(|room| other.len <= room)
    }

    pub fn overlaps(self, other: Self) -> bool {
        self.intersection(other).is_some()
    }

    /// The points both ranges share; `None` if there are none.
    pub fn intersection(self, other: Self) -> Option<Self> {
        let last = self.last()?.min(other.last()?);
        Self::inclusive(self.start.max(other.start), last)
    }

    /// The points before `at` and the rest; `None` unless `at` lies between
    /// the start and the end.
    pub fn split_at(self, at: A) -> Option<(Self, Self)> {
        let offset = at.raw().checked_sub(self.start.raw())?;
        let rest = self.len.checked_sub(offset)?;
        Some((
            Self {
                start: self.start,
                len: offset,
            },
            Self {
                start: at,
                len: rest,
            },
        ))
    }

    /// Whether the start and the end are both multiples of `align`.
    pub fn is_aligned(self, align: Alignment) -> bool {
        self.start.is_aligned(align) && self.len.is_aligned(align)
    }

    /// The largest aligned range within this one; `None` if it would be empty.
    pub fn align_inward(self, align: Alignment) -> Option<Self> {
        let start = self.start.align_up(align)?;
        let len = self
            .len
            .checked_sub(start.raw() - self.start.raw())?
            .align_down(align);
        (len != A::Raw::ZERO).then_some(Self { start, len })
    }

    /// The smallest aligned range this one lies within; `None` if it would run
    /// past the top of the space.
    pub fn align_outward(self, align: Alignment) -> Option<Self> {
        let start = self.start.align_down(align);
        let len = (self.start.raw() - start.raw())
            .checked_add(self.len)?
            .align_up(align)?;
        Self::new(start, len)
    }
}

impl<A: Space> fmt::Display for Range<A> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.last() {
            Some(last) => write!(formatter, "{}..={last}", self.start),
            None => write!(formatter, "{0}..{0}", self.start),
        }
    }
}

impl<A: Space> fmt::Debug for Range<A> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum RangeError {
    /// range ends before it starts
    Reversed,
    /// range covers the whole space, which its length cannot express
    WholeSpace,
}

impl<A: Space> TryFrom<ops::Range<A::Raw>> for Range<A> {
    type Error = RangeError;

    fn try_from(range: ops::Range<A::Raw>) -> Result<Self, RangeError> {
        let len = range
            .end
            .checked_sub(range.start)
            .ok_or(RangeError::Reversed)?;
        Ok(Self {
            start: A::from_raw(range.start),
            len,
        })
    }
}

impl<A: Space> TryFrom<ops::RangeInclusive<A::Raw>> for Range<A> {
    type Error = RangeError;

    fn try_from(range: ops::RangeInclusive<A::Raw>) -> Result<Self, RangeError> {
        let (start, last) = (A::from_raw(*range.start()), A::from_raw(*range.end()));
        if last < start {
            return Err(RangeError::Reversed);
        }
        // Iterating to the end leaves `start == end` but no points.
        if range.is_empty() {
            return Ok(Self {
                start,
                len: A::Raw::ZERO,
            });
        }
        Self::inclusive(start, last).ok_or(RangeError::WholeSpace)
    }
}

macro_rules! spaces {
    ($($(#[$doc:meta])* $name:ident($raw:ty), $format:literal;)*) => {$(
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub $raw);

        impl Space for $name {
            type Raw = $raw;

            fn from_raw(raw: $raw) -> Self {
                Self(raw)
            }

            fn raw(self) -> $raw {
                self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, concat!(stringify!($name), "(", $format, ")"), self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, $format, self.0)
            }
        }

        // Generic `const fn`s cannot call trait methods, so each space gets its own.
        impl Range<$name> {
            /// [`Range::new`] for `const` items, where unwrapping `None` fails
            /// the build.
            pub const fn const_new(start: $name, len: $raw) -> Option<Self> {
                if len == 0 || start.0.checked_add(len - 1).is_some() {
                    Some(Self { start, len })
                } else {
                    None
                }
            }

            /// [`Range::inclusive`] for `const` items, where unwrapping `None`
            /// fails the build.
            pub const fn const_inclusive(start: $name, last: $name) -> Option<Self> {
                match last.0.checked_sub(start.0) {
                    Some(span) if span < <$raw>::MAX => Some(Self { start, len: span + 1 }),
                    _ => None,
                }
            }
        }
    )*};
}

spaces! {
    /// A guest-physical address.
    Gpa(u64), "{:#x}";
    /// An x86 I/O port.
    Pio(u16), "{:#x}";
    /// A global system interrupt: an input line of the guest's interrupt
    /// controllers.
    Gsi(u32), "{}";
    /// A byte offset within a storage object of guest memory.
    ObjectOffset(u64), "{:#x}";
    /// A KVM memory slot. Runtime only: a restore binds memory to whichever
    /// slots the new VM hands out, so slots never appear in a snapshot.
    MemSlot(u32), "{}";
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: Alignment = Alignment::new(0x1000).unwrap();
    const EIGHT: Alignment = Alignment::new(8).unwrap();
    const TOP_PORTS: Range<Pio> = Range::<Pio>::const_inclusive(Pio(0xfff8), Pio(0xffff)).unwrap();
    const TOP_PAGE: Range<Gpa> = Range::<Gpa>::const_new(Gpa(u64::MAX - 0xfff), 0x1000).unwrap();

    fn gpa(range: ops::Range<u64>) -> Range<Gpa> {
        range.try_into().unwrap()
    }

    fn pio(range: ops::RangeInclusive<u16>) -> Range<Pio> {
        range.try_into().unwrap()
    }

    fn empty_pio(start: u16) -> Range<Pio> {
        Range::new(Pio(start), 0).unwrap()
    }

    #[test]
    fn ranges_may_end_at_the_top_of_their_space_but_not_past_it() {
        let ports = [
            (0xfff8, 8, true),
            (0xfff8, 9, false),
            (0xffff, 1, true),
            (0xffff, 2, false),
            (0xffff, 0, true),
            (0, 0xffff, true),
            (1, 0xffff, true),
            (2, 0xffff, false),
        ];
        for (start, len, valid) in ports {
            let range = Range::new(Pio(start), len);
            assert_eq!(range.is_some(), valid, "{start:#x} + {len:#x}");
            assert_eq!(Range::<Pio>::const_new(Pio(start), len), range);
        }
        let addresses = [
            (u64::MAX, 1, true),
            (u64::MAX, 2, false),
            (1, u64::MAX, true),
            (2, u64::MAX, false),
            (u64::MAX, 0, true),
        ];
        for (start, len, valid) in addresses {
            let range = Range::new(Gpa(start), len);
            assert_eq!(range.is_some(), valid, "{start:#x} + {len:#x}");
            assert_eq!(Range::<Gpa>::const_new(Gpa(start), len), range);
        }
    }

    #[test]
    fn inclusive_ranges_cannot_be_reversed_or_cover_the_whole_space() {
        let cases = [
            (0xfff8, 0xffff, Some(8)),
            (0xffff, 0xffff, Some(1)),
            (0, 0xfffe, Some(0xffff)),
            (1, 0xffff, Some(0xffff)),
            (0, 0xffff, None),
            (5, 4, None),
        ];
        for (start, last, len) in cases {
            let range = Range::inclusive(Pio(start), Pio(last));
            assert_eq!(range.map(Range::len), len, "{start:#x}..={last:#x}");
            assert_eq!(Range::<Pio>::const_inclusive(Pio(start), Pio(last)), range);
        }
        assert_eq!(Range::inclusive(Gpa(0), Gpa(u64::MAX)), None);
        assert_eq!(Range::<Gsi>::const_inclusive(Gsi(0), Gsi(u32::MAX)), None);
    }

    #[test]
    fn last_is_inclusive_and_end_is_checked() {
        assert_eq!(TOP_PORTS.start(), Pio(0xfff8));
        assert_eq!(TOP_PORTS.len(), 8);
        assert_eq!(TOP_PORTS.last(), Some(Pio(0xffff)));
        assert_eq!(TOP_PORTS.end(), None);
        assert!(!TOP_PORTS.is_empty());
        assert_eq!(TOP_PAGE.last(), Some(Gpa(u64::MAX)));
        assert_eq!(TOP_PAGE.end(), None);

        let range = gpa(0x1000..0x2000);
        assert_eq!(range.last(), Some(Gpa(0x1fff)));
        assert_eq!(range.end(), Some(Gpa(0x2000)));

        for empty in [empty_pio(0x60), empty_pio(0xffff)] {
            assert!(empty.is_empty());
            assert_eq!(empty.last(), None);
            assert_eq!(empty.end(), Some(empty.start()));
        }
    }

    #[test]
    fn points_are_contained_from_start_through_last() {
        let range = gpa(0x1000..0x2000);
        assert!(range.contains(Gpa(0x1000)));
        assert!(range.contains(Gpa(0x1fff)));
        assert!(!range.contains(Gpa(0xfff)));
        assert!(!range.contains(Gpa(0x2000)));
        assert!(TOP_PORTS.contains(Pio(0xffff)));
        assert!(TOP_PORTS.contains(Pio(0xfff8)));
        assert!(!TOP_PORTS.contains(Pio(0xfff7)));
        assert!(TOP_PAGE.contains(Gpa(u64::MAX)));
        assert!(!empty_pio(0x60).contains(Pio(0x60)));
        assert!(!empty_pio(0xffff).contains(Pio(0xffff)));
    }

    #[test]
    fn contained_ranges_lie_between_start_and_end() {
        let range = gpa(0x1000..0x3000);
        for inner in [
            range,
            gpa(0x1000..0x1001),
            gpa(0x2fff..0x3000),
            gpa(0x1800..0x2800),
            gpa(0x1000..0x1000),
            gpa(0x3000..0x3000),
        ] {
            assert!(range.contains_range(inner), "{inner}");
        }
        for outer in [
            gpa(0xfff..0x1000),
            gpa(0xfff..0x2000),
            gpa(0x2000..0x3001),
            gpa(0x3000..0x3001),
            gpa(0x3001..0x3001),
            gpa(0..0x4000),
        ] {
            assert!(!range.contains_range(outer), "{outer}");
        }

        assert!(TOP_PORTS.contains_range(TOP_PORTS));
        assert!(TOP_PORTS.contains_range(pio(0xffff..=0xffff)));
        assert!(TOP_PORTS.contains_range(empty_pio(0xffff)));
        assert!(!TOP_PORTS.contains_range(pio(0xfff7..=0xffff)));
        assert!(!pio(0xfff0..=0xfffe).contains_range(pio(0xfff8..=0xffff)));
        assert!(pio(0xfff0..=0xfffe).contains_range(empty_pio(0xffff)));
        assert!(TOP_PAGE.contains_range(gpa(u64::MAX - 1..u64::MAX)));
        assert!(empty_pio(0x60).contains_range(empty_pio(0x60)));
        assert!(!empty_pio(0x60).contains_range(pio(0x60..=0x60)));
    }

    #[test]
    fn ranges_overlap_only_when_they_share_an_address() {
        let range = gpa(0x1000..0x2000);
        assert!(range.overlaps(gpa(0x1fff..0x3000)));
        assert!(range.overlaps(gpa(0..0x4000)));
        assert!(range.overlaps(gpa(0..0x1001)));
        assert!(!range.overlaps(gpa(0x2000..0x3000)));
        assert!(!range.overlaps(gpa(0..0x1000)));
        assert!(!range.overlaps(gpa(0x1800..0x1800)));
        assert!(!range.overlaps(gpa(0x1000..0x1000)));

        assert!(TOP_PORTS.overlaps(pio(0xfff0..=0xfff8)));
        assert!(TOP_PORTS.overlaps(pio(0xffff..=0xffff)));
        assert!(!TOP_PORTS.overlaps(pio(0xfff0..=0xfff7)));
        assert!(!TOP_PORTS.overlaps(empty_pio(0xffff)));
        assert!(TOP_PAGE.overlaps(gpa(u64::MAX - 1..u64::MAX)));
    }

    #[test]
    fn intersections_hold_exactly_the_shared_points() {
        let range = gpa(0x1000..0x3000);
        let cases = [
            (range, Some(range)),
            (gpa(0x1800..0x2000), Some(gpa(0x1800..0x2000))),
            (gpa(0..0x1800), Some(gpa(0x1000..0x1800))),
            (gpa(0x2fff..0x4000), Some(gpa(0x2fff..0x3000))),
            (gpa(0..0x1001), Some(gpa(0x1000..0x1001))),
            (gpa(0..0x1000), None),
            (gpa(0x3000..0x4000), None),
            (gpa(0x2000..0x2000), None),
        ];
        for (other, shared) in cases {
            assert_eq!(range.intersection(other), shared, "{other}");
            assert_eq!(other.intersection(range), shared, "{other}");
            assert_eq!(range.overlaps(other), shared.is_some(), "{other}");
        }

        assert_eq!(
            TOP_PORTS.intersection(pio(0xfffc..=0xffff)),
            Some(pio(0xfffc..=0xffff))
        );
        assert_eq!(
            TOP_PORTS.intersection(pio(0xfff0..=0xfff8)),
            Some(pio(0xfff8..=0xfff8))
        );
        assert_eq!(TOP_PORTS.intersection(pio(0..=0xfff7)), None);
        assert_eq!(
            TOP_PAGE.intersection(Range::inclusive(Gpa(1), Gpa(u64::MAX)).unwrap()),
            Some(TOP_PAGE)
        );
    }

    #[test]
    fn splitting_divides_a_range_at_a_point_between_start_and_end() {
        let range = gpa(0x1000..0x3000);
        assert_eq!(
            range.split_at(Gpa(0x1800)),
            Some((gpa(0x1000..0x1800), gpa(0x1800..0x3000)))
        );
        assert_eq!(
            range.split_at(Gpa(0x1000)),
            Some((gpa(0x1000..0x1000), range))
        );
        assert_eq!(
            range.split_at(Gpa(0x3000)),
            Some((range, gpa(0x3000..0x3000)))
        );
        assert_eq!(range.split_at(Gpa(0xfff)), None);
        assert_eq!(range.split_at(Gpa(0x3001)), None);

        assert_eq!(
            TOP_PORTS.split_at(Pio(0xffff)),
            Some((pio(0xfff8..=0xfffe), pio(0xffff..=0xffff)))
        );
        assert_eq!(
            TOP_PORTS.split_at(Pio(0xfff8)),
            Some((empty_pio(0xfff8), TOP_PORTS))
        );
        assert_eq!(
            TOP_PAGE.split_at(Gpa(u64::MAX)),
            Some((
                gpa(u64::MAX - 0xfff..u64::MAX),
                Range::new(Gpa(u64::MAX), 1).unwrap()
            ))
        );
        assert_eq!(
            empty_pio(0x60).split_at(Pio(0x60)),
            Some((empty_pio(0x60), empty_pio(0x60)))
        );
        assert_eq!(empty_pio(0x60).split_at(Pio(0x61)), None);
    }

    #[test]
    fn aligned_ranges_start_and_end_on_multiples() {
        assert!(gpa(0x1000..0x3000).is_aligned(PAGE));
        assert!(gpa(0x1000..0x1000).is_aligned(PAGE));
        assert!(!gpa(0x1800..0x3000).is_aligned(PAGE));
        assert!(!gpa(0x1000..0x2800).is_aligned(PAGE));
        assert!(!gpa(0x1800..0x1800).is_aligned(PAGE));
        assert!(TOP_PORTS.is_aligned(EIGHT));
        assert!(!TOP_PORTS.is_aligned(Alignment::new(16).unwrap()));
        assert!(TOP_PAGE.is_aligned(PAGE));
        assert!(!TOP_PAGE.is_aligned(Alignment::new(0x2000).unwrap()));

        // Wider than the space: only the empty range at 0 is aligned.
        let wide = Alignment::new(1 << 20).unwrap();
        assert!(empty_pio(0).is_aligned(wide));
        assert!(!pio(0..=0xfffe).is_aligned(wide));
        assert!(!TOP_PORTS.is_aligned(wide));
    }

    #[test]
    fn inward_alignment_keeps_the_largest_aligned_part() {
        let cases = [
            (gpa(0x1000..0x3000), Some(gpa(0x1000..0x3000))),
            (gpa(0x1800..0x4800), Some(gpa(0x2000..0x4000))),
            (gpa(0x1001..0x3000), Some(gpa(0x2000..0x3000))),
            (gpa(0x1800..0x2fff), None),
            (gpa(0x1800..0x2000), None),
            (gpa(0x2000..0x2000), None),
        ];
        for (range, inward) in cases {
            assert_eq!(range.align_inward(PAGE), inward, "{range}");
        }

        assert_eq!(pio(0xfff1..=0xffff).align_inward(EIGHT), Some(TOP_PORTS));
        assert_eq!(pio(0xfff9..=0xffff).align_inward(EIGHT), None);
        assert_eq!(TOP_PORTS.align_inward(EIGHT), Some(TOP_PORTS));
        assert_eq!(
            Range::inclusive(Gpa(u64::MAX - 0x17ff), Gpa(u64::MAX))
                .unwrap()
                .align_inward(PAGE),
            Some(TOP_PAGE)
        );
        assert_eq!(
            pio(0..=0xfffe).align_inward(Alignment::new(1 << 20).unwrap()),
            None
        );
    }

    #[test]
    fn outward_alignment_covers_the_range_unless_that_passes_the_top() {
        let cases = [
            (gpa(0x1000..0x3000), Some(gpa(0x1000..0x3000))),
            (gpa(0x1800..0x4800), Some(gpa(0x1000..0x5000))),
            (gpa(0x1fff..0x2001), Some(gpa(0x1000..0x3000))),
            (gpa(0x2000..0x2000), Some(gpa(0x2000..0x2000))),
            (gpa(0x1801..0x1801), Some(gpa(0x1000..0x2000))),
        ];
        for (range, outward) in cases {
            let aligned = range.align_outward(PAGE);
            assert_eq!(aligned, outward, "{range}");
            assert!(aligned.unwrap().contains_range(range));
        }

        assert_eq!(pio(0xfff9..=0xfffa).align_outward(EIGHT), Some(TOP_PORTS));
        assert_eq!(empty_pio(0xffff).align_outward(EIGHT), Some(TOP_PORTS));
        assert_eq!(TOP_PORTS.align_outward(EIGHT), Some(TOP_PORTS));
        assert_eq!(
            pio(1..=0xffff).align_outward(Alignment::new(2).unwrap()),
            None
        );
        assert_eq!(
            gpa(u64::MAX - 1..u64::MAX).align_outward(PAGE),
            Some(TOP_PAGE)
        );
        let wide = Alignment::new(1 << 20).unwrap();
        assert_eq!(empty_pio(0).align_outward(wide), Some(empty_pio(0)));
        assert_eq!(pio(0..=0).align_outward(wide), None);
    }

    #[test]
    fn points_align_without_passing_the_top() {
        assert_eq!(Gpa(0x1001).align_up(PAGE), Some(Gpa(0x2000)));
        assert_eq!(Gpa(0x2000).align_up(PAGE), Some(Gpa(0x2000)));
        assert_eq!(Gpa(0x1fff).align_down(PAGE), Gpa(0x1000));
        assert_eq!(Gpa(u64::MAX).align_up(PAGE), None);
        assert_eq!(Gpa(u64::MAX).align_down(PAGE), Gpa(u64::MAX - 0xfff));
        assert_eq!(Pio(0xfff8).align_up(EIGHT), Some(Pio(0xfff8)));
        assert_eq!(Pio(0xfff9).align_up(EIGHT), None);
        assert!(Gpa(0).is_aligned(PAGE));
        assert!(!Gpa(0x800).is_aligned(PAGE));
        assert!(0x3000u64.is_aligned(PAGE));

        let wide = Alignment::new(1 << 20).unwrap();
        assert_eq!(Pio(0).align_up(wide), Some(Pio(0)));
        assert_eq!(Pio(1).align_up(wide), None);
        assert_eq!(Pio(0xffff).align_down(wide), Pio(0));
        assert!(!Pio(0x8000).is_aligned(wide));
    }

    #[test]
    fn formatting_is_inclusive_and_reaches_the_top() {
        assert_eq!(TOP_PORTS.to_string(), "0xfff8..=0xffff");
        assert_eq!(format!("{TOP_PORTS:?}"), "0xfff8..=0xffff");
        assert_eq!(
            TOP_PAGE.to_string(),
            "0xfffffffffffff000..=0xffffffffffffffff"
        );
        assert_eq!(gpa(0x1000..0x2000).to_string(), "0x1000..=0x1fff");
        assert_eq!(gpa(0x1000..0x1000).to_string(), "0x1000..0x1000");
        assert_eq!(empty_pio(0xffff).to_string(), "0xffff..0xffff");
        assert_eq!(
            Range::inclusive(Gsi(5), Gsi(23)).unwrap().to_string(),
            "5..=23"
        );
        assert_eq!(format!("{:?}", Gpa(0x1000)), "Gpa(0x1000)");
        assert_eq!(format!("{:?}", Pio(0x3f8)), "Pio(0x3f8)");
        assert_eq!(format!("{:?}", Gsi(4)), "Gsi(4)");
        assert_eq!(Gsi(4).to_string(), "4");
        assert_eq!(
            format!("{:?}", ObjectOffset(0x2000)),
            "ObjectOffset(0x2000)"
        );
        assert_eq!(
            Range::new(ObjectOffset(0x2000), 0x1000)
                .unwrap()
                .to_string(),
            "0x2000..=0x2fff"
        );
        assert_eq!(format!("{:?}", MemSlot(3)), "MemSlot(3)");
        assert_eq!(MemSlot(3).to_string(), "3");
    }

    #[test]
    fn std_ranges_convert_when_representable() {
        assert_eq!(Range::<Pio>::try_from(0x60..0x65), Ok(pio(0x60..=0x64)));
        assert_eq!(Range::<Pio>::try_from(0x60..0x60), Ok(empty_pio(0x60)));
        assert_eq!(
            Range::<Pio>::try_from(ops::Range {
                start: 0x65,
                end: 0x60
            }),
            Err(RangeError::Reversed)
        );
        assert_eq!(Range::<Pio>::try_from(0xfff8..=0xffff), Ok(TOP_PORTS));
        assert_eq!(
            Range::<Pio>::try_from(0..=0xffff),
            Err(RangeError::WholeSpace)
        );
        assert_eq!(
            Range::<Pio>::try_from(ops::RangeInclusive::new(0x60, 0x5f)),
            Err(RangeError::Reversed)
        );
        assert_eq!(
            Range::<Gpa>::try_from(0..=u64::MAX),
            Err(RangeError::WholeSpace)
        );
        assert_eq!(
            Range::<Gpa>::try_from(u64::MAX - 0xfff..=u64::MAX),
            Ok(TOP_PAGE)
        );
        let mut exhausted = 0x60..=0x61;
        exhausted.by_ref().for_each(drop);
        assert_eq!(Range::<Pio>::try_from(exhausted), Ok(empty_pio(0x61)));
        assert_eq!(
            RangeError::WholeSpace.to_string(),
            "range covers the whole space, which its length cannot express"
        );
    }
}
