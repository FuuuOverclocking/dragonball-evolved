//! Guest I/O buses: which device answers an access to a port or an address.
//!
//! A bus is built once and never changes, so vCPU threads dispatch through
//! `&self` without locks of their own and without allocating.

use std::fmt;
use std::sync::Arc;

use crate::space::{Gpa, Pio, Range, Space};

/// A device on a bus, accessed at an offset within its range from any vCPU
/// thread. Any locking is the device's own.
pub trait BusDevice<A: Space>: Send + Sync {
    fn read(&self, offset: A::Raw, data: &mut [u8]);
    fn write(&self, offset: A::Raw, data: &[u8]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum BusError<A: Space> {
    /// bus range {0} is empty
    Empty(Range<A>),
    /// bus ranges {0} and {1} overlap
    Overlap(Range<A>, Range<A>),
}

struct Entry<A: Space> {
    range: Range<A>,
    device: Arc<dyn BusDevice<A>>,
}

/// Disjoint, non-empty device ranges of one space, sorted by start.
pub struct Bus<A: Space> {
    entries: Box<[Entry<A>]>,
}

pub type PioBus = Bus<Pio>;
pub type MmioBus = Bus<Gpa>;

impl<A: Space> Bus<A> {
    pub fn new(mut devices: Vec<(Range<A>, Arc<dyn BusDevice<A>>)>) -> Result<Self, BusError<A>> {
        if let Some((range, _)) = devices.iter().find(|(range, _)| range.is_empty()) {
            return Err(BusError::Empty(*range));
        }
        devices.sort_unstable_by_key(|(range, _)| range.start());
        if let Some(pair) = devices
            .windows(2)
            .find(|pair| pair[0].0.overlaps(pair[1].0))
        {
            return Err(BusError::Overlap(pair[0].0, pair[1].0));
        }
        let entries = devices
            .into_iter()
            .map(|(range, device)| Entry { range, device })
            .collect();
        Ok(Self { entries })
    }

    /// Dispatches a read; `false` if no single device covers the access.
    pub fn read(&self, address: A, data: &mut [u8]) -> bool {
        let Some((entry, offset)) = self.find(address, data.len()) else {
            return false;
        };
        entry.device.read(offset, data);
        true
    }

    /// Dispatches a write; `false` if no single device covers the access.
    pub fn write(&self, address: A, data: &[u8]) -> bool {
        let Some((entry, offset)) = self.find(address, data.len()) else {
            return false;
        };
        entry.device.write(offset, data);
        true
    }

    fn find(&self, address: A, len: usize) -> Option<(&Entry<A>, A::Raw)> {
        let access = Range::new(address, A::Raw::try_from(len).ok()?)?;
        if access.is_empty() {
            return None;
        }
        let index = self
            .entries
            .partition_point(|entry| entry.range.start() <= address)
            .checked_sub(1)?;
        let entry = &self.entries[index];
        entry
            .range
            .contains_range(access)
            .then(|| (entry, address.raw() - entry.range.start().raw()))
    }
}

impl<A: Space> Default for Bus<A> {
    fn default() -> Self {
        Self {
            entries: Box::new([]),
        }
    }
}

impl<A: Space> fmt::Debug for Bus<A> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_list()
            .entries(self.entries.iter().map(|entry| &entry.range))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::marker::PhantomData;
    use std::ops;
    use std::sync::Mutex;

    use super::*;

    /// Reads its offset back and records writes.
    struct Device<A>(Mutex<Vec<(u64, Vec<u8>)>>, PhantomData<A>);

    impl<A> Default for Device<A> {
        fn default() -> Self {
            Self(Mutex::default(), PhantomData)
        }
    }

    impl<A: Space> BusDevice<A> for Device<A>
    where
        A::Raw: Into<u64>,
        PhantomData<A>: Send + Sync,
    {
        fn read(&self, offset: A::Raw, data: &mut [u8]) {
            data.fill(offset.into() as u8);
        }

        fn write(&self, offset: A::Raw, data: &[u8]) {
            self.0.lock().unwrap().push((offset.into(), data.to_vec()));
        }
    }

    fn gpa(range: ops::Range<u64>) -> Range<Gpa> {
        range.try_into().unwrap()
    }

    fn pio(range: ops::RangeInclusive<u16>) -> Range<Pio> {
        range.try_into().unwrap()
    }

    #[test]
    fn sorts_and_dispatches_ranges() {
        let first = Arc::new(Device::<Gpa>::default());
        let second = Arc::new(Device::<Gpa>::default());
        let bus = Bus::new(vec![
            (
                gpa(0x2000..0x2100),
                second.clone() as Arc<dyn BusDevice<Gpa>>,
            ),
            (gpa(0x1000..0x1100), first.clone()),
        ])
        .unwrap();
        assert_eq!(format!("{bus:?}"), "[0x1000..=0x10ff, 0x2000..=0x20ff]");
        let mut data = [0; 2];
        assert!(bus.read(Gpa(0x1004), &mut data));
        assert_eq!(data, [4, 4]);
        assert!(bus.write(Gpa(0x20fe), &[1, 2]));
        assert_eq!(second.0.lock().unwrap().as_slice(), &[(0xfe, vec![1, 2])]);
        assert!(first.0.lock().unwrap().is_empty());
        // Straddling the end, between ranges, below the first, and empty.
        assert!(!bus.write(Gpa(0x20ff), &[1, 2]));
        assert!(!bus.read(Gpa(0x1800), &mut data));
        assert!(!bus.read(Gpa(0xfff), &mut data));
        assert!(!bus.read(Gpa(0x1000), &mut []));
        assert!(!bus.read(Gpa(u64::MAX), &mut data));
        assert_eq!(second.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn dispatches_a_range_at_the_top_of_the_port_space() {
        let top = Arc::new(Device::<Pio>::default());
        let bus = Bus::new(vec![
            (pio(0xfff8..=0xffff), top.clone() as Arc<dyn BusDevice<Pio>>),
            (pio(0x60..=0x64), Arc::new(Device::<Pio>::default())),
        ])
        .unwrap();
        let mut data = [0; 2];
        assert!(bus.read(Pio(0xfffe), &mut data));
        assert_eq!(data, [6, 6]);
        let mut byte = [0];
        assert!(bus.read(Pio(0xffff), &mut byte));
        assert_eq!(byte, [7]);
        assert!(bus.write(Pio(0xffff), &[9]));
        assert_eq!(top.0.lock().unwrap().as_slice(), &[(7, vec![9])]);
        // Past the top of the space.
        assert!(!bus.read(Pio(0xffff), &mut data));
        assert!(!bus.write(Pio(0xfff7), &[1]));
        assert!(bus.read(Pio(0x64), &mut byte));
        assert!(!bus.read(Pio(0x64), &mut data));
    }

    #[test]
    fn rejects_empty_and_overlapping_ranges() {
        let device: Arc<dyn BusDevice<Gpa>> = Arc::new(Device::<Gpa>::default());
        let empty = gpa(1..1);
        assert_eq!(
            Bus::new(vec![(empty, Arc::clone(&device))]).unwrap_err(),
            BusError::Empty(empty)
        );
        assert_eq!(
            Bus::new(vec![
                (gpa(19..30), Arc::clone(&device)),
                (gpa(10..20), Arc::clone(&device)),
            ])
            .unwrap_err(),
            BusError::Overlap(gpa(10..20), gpa(19..30))
        );
        assert!(
            Bus::new(vec![
                (gpa(10..20), Arc::clone(&device)),
                (gpa(20..30), device),
            ])
            .is_ok()
        );
        assert_eq!(
            BusError::Overlap(pio(0x60..=0x64), pio(0x64..=0x64)).to_string(),
            "bus ranges 0x60..=0x64 and 0x64..=0x64 overlap"
        );
        assert!(!Bus::<Pio>::default().read(Pio(0), &mut [0]));
    }
}
