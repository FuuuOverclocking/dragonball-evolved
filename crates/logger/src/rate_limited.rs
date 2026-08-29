// Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Derived from Firecracker `src/vmm/src/logger/rate_limited.rs`. The state
// encoding and the GCRA arithmetic are kept as they are; the suppression
// notice moved out to the caller so that it carries the flooding callsite
// instead of this file.

//! Per-callsite, lock-free rate limiter for logging.
//!
//! # Algorithm
//!
//! Generic Cell Rate Algorithm, stored in a single `AtomicU64`:
//!
//! ```text
//! bit 63                                             bit 0
//!  ┌──────────────────┬─────────────────────────────────────┐
//!  │  suppressed (24) │            tat_ms (40)              │
//!  └──────────────────┴─────────────────────────────────────┘
//! ```
//!
//! - `tat_ms`: theoretical arrival time, ms since the process epoch.
//!   40 bits is about 34 years before it wraps.
//! - `suppressed`: saturating count of denied calls still awaiting a report.
//!
//! Each call takes `earliest = max(tat, now)` and `new_tat = earliest + T`
//! where `T = REFILL_MS / BURST`. It denies when `new_tat - now > REFILL_MS`
//! and otherwise CAS-advances `tat`. That is a token bucket of capacity
//! `BURST` refilling `BURST` tokens every `REFILL_MS`, with one word of state.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Messages allowed per refill period.
pub const DEFAULT_BURST: u64 = 10;

/// Refill period in milliseconds.
pub const DEFAULT_REFILL_MS: u64 = 5_000;

/// Reference point for `tat_ms`.
static EPOCH: OnceLock<Instant> = OnceLock::new();

fn now_ms_since_epoch() -> u64 {
    let epoch = *EPOCH.get_or_init(Instant::now);
    let elapsed = Instant::now().saturating_duration_since(epoch);
    elapsed.as_secs() * 1_000 + u64::from(elapsed.subsec_millis())
}

/// What a limiter decided about one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Emit the record. `suppressed` is how many calls were denied since the
    /// last emit and have not been reported yet.
    Emit { suppressed: u64 },
    /// Drop the record.
    Deny,
}

/// Lock-free rate limiter over a burst capacity and a refill window. See the
/// [module docs](self) for the state layout.
#[derive(Debug)]
pub struct LogRateLimiter<const BURST: u64, const REFILL_MS: u64> {
    state: AtomicU64,
}

/// The limiter the rate-limited macros install at every callsite.
pub type DefaultLogRateLimiter = LogRateLimiter<DEFAULT_BURST, DEFAULT_REFILL_MS>;

impl<const BURST: u64, const REFILL_MS: u64> Default for LogRateLimiter<BURST, REFILL_MS> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const BURST: u64, const REFILL_MS: u64> LogRateLimiter<BURST, REFILL_MS> {
    const TAT_BITS: u32 = 40;
    const TAT_MASK: u64 = (1 << Self::TAT_BITS) - 1;
    const MAX_SUPPRESSED: u64 = u64::MAX >> Self::TAT_BITS;

    /// Bounds scheduler-induced livelock. Exhausting the retries denies.
    const CAS_RETRY_LIMIT: u32 = 16;

    /// Milliseconds per token. Checked at const-eval time so a callsite whose
    /// parameters round down to zero fails to compile.
    const PERIOD_PER_TOKEN_MS: u64 = {
        assert!(BURST > 0, "LogRateLimiter BURST must be > 0");
        assert!(REFILL_MS > 0, "LogRateLimiter REFILL_MS must be > 0");
        let period = REFILL_MS / BURST;
        assert!(
            period > 0,
            "LogRateLimiter REFILL_MS / BURST must be at least 1 ms"
        );
        period
    };

    /// Largest gap between `tat` and `now` that still admits a call.
    const MAX_DEFICIT_MS: u64 = REFILL_MS;

    const fn pack(tat_ms: u64, suppressed: u64) -> u64 {
        let suppressed = if suppressed > Self::MAX_SUPPRESSED {
            Self::MAX_SUPPRESSED
        } else {
            suppressed
        };
        (suppressed << Self::TAT_BITS) | (tat_ms & Self::TAT_MASK)
    }

    const fn unpack(state: u64) -> (u64, u64) {
        (state & Self::TAT_MASK, state >> Self::TAT_BITS)
    }

    /// A fresh limiter. `const` so it can sit in a `static`.
    pub const fn new() -> Self {
        // Force monomorphisation of the const asserts above.
        let _ = Self::PERIOD_PER_TOKEN_MS;
        let _ = Self::MAX_DEFICIT_MS;
        Self {
            state: AtomicU64::new(0),
        }
    }

    /// Advance the bucket and report what the caller should do.
    ///
    /// Emitting clears the suppressed counter, so the count comes back exactly
    /// once and the caller owes a report for it.
    #[inline(never)]
    pub fn check(&self) -> Verdict {
        let now_ms = now_ms_since_epoch();

        for _ in 0..Self::CAS_RETRY_LIMIT {
            let state = self.state.load(Ordering::Relaxed);
            let (tat_ms, suppressed) = Self::unpack(state);

            let earliest = tat_ms.max(now_ms);
            let new_tat_ms = earliest.saturating_add(Self::PERIOD_PER_TOKEN_MS);
            let denied = new_tat_ms.saturating_sub(now_ms) > Self::MAX_DEFICIT_MS;

            let new_state = if denied {
                Self::pack(tat_ms, suppressed.saturating_add(1))
            } else {
                Self::pack(new_tat_ms, 0)
            };

            if self
                .state
                .compare_exchange_weak(state, new_state, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return if denied {
                    Verdict::Deny
                } else {
                    Verdict::Emit { suppressed }
                };
            }
        }

        Verdict::Deny
    }

    #[cfg(test)]
    fn suppressed(&self) -> u64 {
        Self::unpack(self.state.load(Ordering::Relaxed)).1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Limiter = DefaultLogRateLimiter;

    fn emitted(verdict: Verdict) -> bool {
        matches!(verdict, Verdict::Emit { .. })
    }

    #[test]
    fn burst_is_capped() {
        let limiter = Limiter::default();

        for _ in 0..DEFAULT_BURST {
            assert!(emitted(limiter.check()));
        }
        assert_eq!(limiter.check(), Verdict::Deny);
        assert_eq!(limiter.check(), Verdict::Deny);
    }

    #[test]
    fn callsites_are_independent() {
        let a = Limiter::default();
        let b = Limiter::default();

        for _ in 0..DEFAULT_BURST {
            a.check();
        }

        assert_eq!(a.check(), Verdict::Deny);
        assert!(emitted(b.check()));
    }

    #[test]
    fn refills_over_time() {
        let limiter: LogRateLimiter<2, 100> = LogRateLimiter::new();

        for _ in 0..2 {
            assert!(emitted(limiter.check()));
        }
        assert_eq!(limiter.check(), Verdict::Deny);

        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(emitted(limiter.check()));
    }

    #[test]
    fn suppressed_count_is_reported_once() {
        let limiter: LogRateLimiter<1, 50> = LogRateLimiter::new();

        assert_eq!(limiter.check(), Verdict::Emit { suppressed: 0 });
        for _ in 0..3 {
            assert_eq!(limiter.check(), Verdict::Deny);
        }
        assert_eq!(limiter.suppressed(), 3);

        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(limiter.check(), Verdict::Emit { suppressed: 3 });
        assert_eq!(limiter.suppressed(), 0);

        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(limiter.check(), Verdict::Emit { suppressed: 0 });
    }

    #[test]
    fn suppressed_saturates() {
        type Tight = LogRateLimiter<1, 100>;
        let limiter = Tight::new();

        // Seed a near-saturated state instead of denying 16M times.
        assert!(emitted(limiter.check()));
        let (tat_ms, _) = Tight::unpack(limiter.state.load(Ordering::Relaxed));
        limiter.state.store(
            Tight::pack(tat_ms, Tight::MAX_SUPPRESSED - 1),
            Ordering::Relaxed,
        );

        for _ in 0..5 {
            assert_eq!(limiter.check(), Verdict::Deny);
        }
        assert_eq!(limiter.suppressed(), Tight::MAX_SUPPRESSED);
    }

    #[test]
    fn pack_unpack_roundtrip() {
        type L = DefaultLogRateLimiter;

        for &(tat, suppressed) in &[
            (0, 0),
            (1, 0),
            (0, 1),
            (123_456_789, 42),
            (L::TAT_MASK, 0),
            (0, L::MAX_SUPPRESSED),
            (L::TAT_MASK, L::MAX_SUPPRESSED),
        ] {
            assert_eq!(L::unpack(L::pack(tat, suppressed)), (tat, suppressed));
        }

        assert_eq!(
            L::unpack(L::pack(0, L::MAX_SUPPRESSED + 1)).1,
            L::MAX_SUPPRESSED
        );
    }
}
