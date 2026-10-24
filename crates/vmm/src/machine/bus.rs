use std::fmt;
use std::range::Range;
use std::sync::Arc;

use thiserror::Error;

pub trait BusDevice: Send + Sync {
    fn read(&self, offset: u64, data: &mut [u8]);
    fn write(&self, offset: u64, data: &[u8]);
}

struct Entry {
    range: Range<u64>,
    device: Arc<dyn BusDevice>,
}

pub struct Bus {
    entries: Box<[Entry]>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BusError {
    #[error("bus range {start:#x}..{end:#x} is empty")]
    Empty { start: u64, end: u64 },
    #[error(
        "bus ranges {left_start:#x}..{left_end:#x} and {right_start:#x}..{right_end:#x} overlap"
    )]
    Overlap {
        left_start: u64,
        left_end: u64,
        right_start: u64,
        right_end: u64,
    },
}

impl Bus {
    pub fn new(mut devices: Vec<(Range<u64>, Arc<dyn BusDevice>)>) -> Result<Self, BusError> {
        devices.sort_unstable_by_key(|(range, _)| range.start);
        for (range, _) in &devices {
            if range.is_empty() {
                return Err(BusError::Empty {
                    start: range.start,
                    end: range.end,
                });
            }
        }
        for pair in devices.windows(2) {
            let (left, right) = (pair[0].0, pair[1].0);
            if left.end > right.start {
                return Err(BusError::Overlap {
                    left_start: left.start,
                    left_end: left.end,
                    right_start: right.start,
                    right_end: right.end,
                });
            }
        }
        let entries = devices
            .into_iter()
            .map(|(range, device)| Entry { range, device })
            .collect();
        Ok(Self { entries })
    }

    pub fn empty() -> Self {
        Self {
            entries: Box::new([]),
        }
    }

    pub fn read(&self, address: u64, data: &mut [u8]) -> bool {
        let Some((entry, offset)) = self.find(address, data.len()) else {
            return false;
        };
        entry.device.read(offset, data);
        true
    }

    pub fn write(&self, address: u64, data: &[u8]) -> bool {
        let Some((entry, offset)) = self.find(address, data.len()) else {
            return false;
        };
        entry.device.write(offset, data);
        true
    }

    fn find(&self, address: u64, len: usize) -> Option<(&Entry, u64)> {
        let end = address.checked_add(len as u64)?;
        let index = self
            .entries
            .partition_point(|entry| entry.range.start <= address)
            .checked_sub(1)?;
        let entry = &self.entries[index];
        (end <= entry.range.end).then_some((entry, address - entry.range.start))
    }
}

impl fmt::Debug for Bus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_list()
            .entries(self.entries.iter().map(|entry| &entry.range))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Device(Mutex<Vec<(u64, Vec<u8>)>>);

    impl BusDevice for Device {
        fn read(&self, offset: u64, data: &mut [u8]) {
            data.fill(offset as u8);
        }

        fn write(&self, offset: u64, data: &[u8]) {
            self.0.lock().unwrap().push((offset, data.to_vec()));
        }
    }

    #[test]
    fn sorts_and_dispatches_ranges() {
        let first = Arc::new(Device::default());
        let second = Arc::new(Device::default());
        let bus = Bus::new(vec![
            (Range::from(0x2000..0x2100), second.clone()),
            (Range::from(0x1000..0x1100), first.clone()),
        ])
        .unwrap();
        let mut data = [0; 2];
        assert!(bus.read(0x1004, &mut data));
        assert_eq!(data, [4, 4]);
        assert!(bus.write(0x20fe, &[1, 2]));
        assert_eq!(second.0.lock().unwrap().as_slice(), &[(0xfe, vec![1, 2])]);
        assert!(!bus.write(0x20ff, &[1, 2]));
        assert!(!bus.read(0x1800, &mut data));
    }

    #[test]
    fn rejects_empty_and_overlapping_ranges() {
        let device: Arc<dyn BusDevice> = Arc::new(Device::default());
        assert_eq!(
            Bus::new(vec![(Range::from(1..1), Arc::clone(&device))]).unwrap_err(),
            BusError::Empty { start: 1, end: 1 }
        );
        assert_eq!(
            Bus::new(vec![
                (Range::from(10..20), Arc::clone(&device)),
                (Range::from(19..30), device),
            ])
            .unwrap_err(),
            BusError::Overlap {
                left_start: 10,
                left_end: 20,
                right_start: 19,
                right_end: 30,
            }
        );
    }
}
