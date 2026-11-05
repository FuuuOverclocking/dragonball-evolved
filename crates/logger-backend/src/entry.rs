//! The owned form of a `log::Record`.

use std::fmt::Write as _;

use log::{Level, Record};

use crate::time::Timestamp;

/// Bytes reserved for message, target and file together. `format!` sizes its
/// buffer from the literal parts of the format string alone and reallocates
/// whenever an interpolated value overruns that guess; reserving up front keeps
/// the entry at exactly one allocation.
const RESERVE: usize = 256;

thread_local! {
    /// One `gettid` per thread. The writer needs the id to look the name up.
    static TID: u32 = current_tid();
}

fn current_tid() -> u32 {
    rustix::thread::gettid().as_raw_nonzero().get() as u32
}

/// The caller's thread id, cached after the first log call.
pub fn tid() -> u32 {
    TID.with(|tid| *tid)
}

/// A record detached from the caller's stack.
///
/// `log::Record` borrows the caller's `fmt::Arguments`, so the message must be
/// materialised before the record can cross a thread boundary. `target` and
/// `file` ride behind the message in that same allocation: `Record` does not
/// promise they are `&'static`, and copying them costs a memcpy rather than two
/// more allocations.
#[derive(Debug)]
pub struct Entry {
    pub timestamp: Timestamp,
    /// Records the queue lost before this one, reported alongside it.
    pub dropped: u64,
    pub level: Level,
    pub tid: u32,
    /// Zero when the record carries no line number.
    pub line: u32,
    buf: String,
    message_len: u32,
    target_len: u32,
}

impl Entry {
    pub fn new(record: &Record<'_>, dropped: u64) -> Self {
        let target = record.target();
        let file = record.file().unwrap_or_default();

        let mut buf = String::with_capacity(RESERVE);
        // `fmt::Write` on a `String` is infallible.
        let _ = write!(buf, "{}", record.args());
        let message_len = buf.len() as u32;
        buf.push_str(target);
        buf.push_str(file);

        Self {
            timestamp: Timestamp::now(),
            dropped,
            level: record.level(),
            tid: tid(),
            line: record.line().unwrap_or_default(),
            buf,
            message_len,
            target_len: target.len() as u32,
        }
    }

    pub fn message(&self) -> &str {
        &self.buf[..self.message_len as usize]
    }

    pub fn target(&self) -> &str {
        let start = self.message_len as usize;
        &self.buf[start..start + self.target_len as usize]
    }

    /// Empty when the record carries no file name.
    pub fn file(&self) -> &str {
        &self.buf[(self.message_len + self.target_len) as usize..]
    }
}

#[cfg(test)]
mod tests {
    use log::{Level, MetadataBuilder};

    use super::*;

    fn entry(message: &str, target: &str, file: Option<&str>) -> Entry {
        // One expression on purpose: `format_args!` and the metadata are
        // temporaries that only live to the end of the enclosing statement.
        Entry::new(
            &Record::builder()
                .metadata(
                    MetadataBuilder::new()
                        .level(Level::Warn)
                        .target(target)
                        .build(),
                )
                .args(format_args!("{message}"))
                .file(file)
                .line(Some(42))
                .build(),
            0,
        )
    }

    #[test]
    fn slices_the_shared_buffer() {
        let entry = entry("disk is on fire", "vmm::devices::block", Some("block.rs"));

        assert_eq!(entry.message(), "disk is on fire");
        assert_eq!(entry.target(), "vmm::devices::block");
        assert_eq!(entry.file(), "block.rs");
        assert_eq!(entry.line, 42);
        assert_eq!(entry.level, Level::Warn);
    }

    #[test]
    fn tolerates_missing_and_empty_parts() {
        let entry = entry("", "", None);

        assert_eq!(entry.message(), "");
        assert_eq!(entry.target(), "");
        assert_eq!(entry.file(), "");
    }

    #[test]
    fn keeps_char_boundaries_for_multibyte_text() {
        let entry = entry("设备失败", "vmm::设备", Some("块.rs"));

        assert_eq!(entry.message(), "设备失败");
        assert_eq!(entry.target(), "vmm::设备");
        assert_eq!(entry.file(), "块.rs");
    }

    #[test]
    fn oversized_message_still_slices() {
        let long = "x".repeat(RESERVE * 3);
        let entry = entry(&long, "t", Some("f"));

        assert_eq!(entry.message(), long);
        assert_eq!(entry.target(), "t");
        assert_eq!(entry.file(), "f");
    }

    #[test]
    fn tid_is_stable_within_a_thread() {
        assert_eq!(tid(), tid());
        let other = std::thread::spawn(tid).join().unwrap();
        assert_ne!(tid(), other);
    }
}
