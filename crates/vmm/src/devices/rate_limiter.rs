//! Token bucket rate limiting.
//!
//! Firecracker gives every rate-limited device a `timerfd`, an epoll
//! registration and a dispatch tag, because the only way to wake a subscriber
//! later is to make it wait on a descriptor. A device that owns its control flow
//! can simply sleep, so all this type has to answer is *when* to wake up.

use std::time::Duration;

use tokio::time::Instant;

/// Whether a device may spend from its budget right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// The operation may proceed.
    Available,
    /// Not enough tokens. Retry no earlier than this instant.
    Exhausted { refilled_at: Instant },
}

/// A token bucket over operation counts.
///
/// Bandwidth limiting has exactly this shape with the cost measured in bytes;
/// only IOPS is modelled here to keep the proof of concept small.
#[derive(Debug)]
pub struct TokenBucket {
    /// Tokens replenished every `refill_time`, and the maximum burst size.
    capacity: u64,
    available: u64,
    refill_time: Duration,
    last_refill: Instant,
}

impl TokenBucket {
    /// A bucket that grants `capacity` operations per `refill_time`.
    pub fn new(capacity: u64, refill_time: Duration) -> Self {
        assert!(capacity > 0, "a token bucket needs a non-zero capacity");
        assert!(
            !refill_time.is_zero(),
            "a token bucket needs a non-zero refill time"
        );
        Self {
            capacity,
            available: capacity,
            refill_time,
            last_refill: Instant::now(),
        }
    }

    /// Spend `tokens` if the budget allows it.
    pub fn consume(&mut self, tokens: u64) -> Budget {
        self.refill();
        if self.available >= tokens {
            self.available -= tokens;
            return Budget::Available;
        }
        let missing = tokens.saturating_sub(self.available);
        Budget::Exhausted {
            refilled_at: self.last_refill + self.time_to_accrue(missing),
        }
    }

    fn refill(&mut self) {
        let elapsed = self.last_refill.elapsed();
        let gained = u64::try_from(
            elapsed.as_nanos() * u128::from(self.capacity) / self.refill_time.as_nanos(),
        )
        .unwrap_or(u64::MAX);

        // Leaving `last_refill` alone while `gained` rounds down to zero is what
        // keeps the fractional remainder, so a device polling in a tight loop
        // still accrues tokens at the configured rate.
        if gained > 0 {
            self.available = self.capacity.min(self.available.saturating_add(gained));
            self.last_refill += self
                .refill_time
                .mul_f64(gained as f64 / self.capacity as f64);
        }
    }

    fn time_to_accrue(&self, tokens: u64) -> Duration {
        self.refill_time
            .mul_f64(tokens.min(self.capacity) as f64 / self.capacity as f64)
    }
}
