//! Wall-clock timestamps, rendered without touching the allocator.
//!
//! `localtime_r` is not async-signal-safe, so the UTC offset is read once at
//! init and every later conversion is integer arithmetic. A process that
//! outlives a DST transition keeps reporting the offset it started with.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::buf::Buf;

/// `YYYY-MM-DDThh:mm:ss.ssssss+hh:mm` needs 32 bytes.
pub type Rendered = Buf<40>;

static UTC_OFFSET_SECS: AtomicI64 = AtomicI64::new(0);

/// Read the local UTC offset and remember it. Call once, before any handler is
/// installed.
pub fn capture_utc_offset() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };

    // SAFETY: `now` is a plain value and `tm` is a live, aligned, exclusively
    // borrowed `libc::tm`; `localtime_r` writes only through the out pointer.
    let filled = unsafe { !libc::localtime_r(&now, &mut tm).is_null() };
    if filled {
        UTC_OFFSET_SECS.store(tm.tm_gmtoff as i64, Ordering::Relaxed);
    }
}

/// A point on the wall clock, in microseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(u64);

impl Timestamp {
    /// Reads `CLOCK_REALTIME`, which is async-signal-safe.
    pub fn now() -> Self {
        Self(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as u64,
        )
    }

    #[cfg(test)]
    const fn from_micros(micros: u64) -> Self {
        Self(micros)
    }
}

/// Render as local time with an explicit offset, or `Z` when the offset is zero.
pub fn render(timestamp: Timestamp) -> Rendered {
    let offset_secs = UTC_OFFSET_SECS.load(Ordering::Relaxed);
    let micros = timestamp.0 as i64 + offset_secs * 1_000_000;
    let secs = micros.div_euclid(1_000_000);
    let subsec_micros = micros.rem_euclid(1_000_000) as u64;
    let secs_of_day = secs.rem_euclid(86_400) as u64;
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));

    let mut out = Rendered::new();
    out.push_u64_pad(year.max(0) as u64, 4);
    out.push(b'-');
    out.push_u64_pad(month, 2);
    out.push(b'-');
    out.push_u64_pad(day, 2);
    out.push(b'T');
    out.push_u64_pad(secs_of_day / 3_600, 2);
    out.push(b':');
    out.push_u64_pad((secs_of_day / 60) % 60, 2);
    out.push(b':');
    out.push_u64_pad(secs_of_day % 60, 2);
    out.push(b'.');
    out.push_u64_pad(subsec_micros, 6);

    if offset_secs == 0 {
        out.push(b'Z');
    } else {
        out.push(if offset_secs < 0 { b'-' } else { b'+' });
        let magnitude = offset_secs.unsigned_abs();
        out.push_u64_pad(magnitude / 3_600, 2);
        out.push(b':');
        out.push_u64_pad((magnitude / 60) % 60, 2);
    }

    out
}

/// Convert days since the Unix epoch into a proleptic Gregorian date.
///
/// Howard Hinnant's `civil_from_days`, which shifts the era so that leap days
/// land at the end of a 400-year cycle:
/// https://howardhinnant.github.io/date_algorithms.html#civil_from_days
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;

    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era as i64 + era * 400 + i64::from(month <= 2);

    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    /// Every test here rewrites the process-wide offset, so they cannot run
    /// concurrently with each other.
    static OFFSET: Mutex<()> = Mutex::new(());

    fn exclusive() -> MutexGuard<'static, ()> {
        OFFSET.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn rendered_at(offset_secs: i64, secs: u64, micros: u32) -> String {
        UTC_OFFSET_SECS.store(offset_secs, Ordering::Relaxed);
        let stamp = Timestamp::from_micros(secs * 1_000_000 + u64::from(micros));
        render(stamp).as_str().to_owned()
    }

    #[test]
    fn renders_utc() {
        let _guard = exclusive();

        assert_eq!(rendered_at(0, 0, 0), "1970-01-01T00:00:00.000000Z");
        assert_eq!(
            rendered_at(0, 1_788_170_072, 123_456),
            "2026-08-31T09:54:32.123456Z"
        );
        assert_eq!(
            rendered_at(0, 1_709_208_000, 0),
            "2024-02-29T12:00:00.000000Z"
        );
        // 2000 is a leap year (divisible by 400), 2100 is not (divisible by 100).
        assert_eq!(
            rendered_at(0, 951_782_400, 0),
            "2000-02-29T00:00:00.000000Z"
        );
        assert_eq!(
            rendered_at(0, 4_107_542_400, 0),
            "2100-03-01T00:00:00.000000Z"
        );
    }

    #[test]
    fn renders_offsets() {
        let _guard = exclusive();

        assert_eq!(
            rendered_at(8 * 3_600, 1_788_170_072, 123_456),
            "2026-08-31T17:54:32.123456+08:00"
        );
        // Half-hour offsets have to survive the minutes field.
        assert_eq!(
            rendered_at(5 * 3_600 + 1_800, 1_788_170_072, 0),
            "2026-08-31T15:24:32.000000+05:30"
        );
        assert_eq!(
            rendered_at(-7 * 3_600, 1_788_170_072, 0),
            "2026-08-31T02:54:32.000000-07:00"
        );
    }

    #[test]
    fn negative_offset_crosses_the_epoch() {
        let _guard = exclusive();

        // Local time lands before 1970, so the date maths has to stay signed.
        assert_eq!(
            rendered_at(-3_600, 0, 0),
            "1969-12-31T23:00:00.000000-01:00"
        );
    }

    #[test]
    fn captured_offset_matches_libc() {
        let _guard = exclusive();

        capture_utc_offset();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&now, &mut tm) };

        assert_eq!(UTC_OFFSET_SECS.load(Ordering::Relaxed), tm.tm_gmtoff as i64);
    }
}
