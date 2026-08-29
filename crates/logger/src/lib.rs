//! Logging frontend.
//!
//! Every crate logs through the macros here; only the bin crate knows where the
//! records end up. This crate stays a leaf on purpose — it depends on nothing
//! but `log`, so a library does not drag in files, threads or serialisation.
//!
//! `error!`, `warn!` and `info!` are rate limited per callsite, which is what
//! any guest-triggerable path must use. `*_unrestricted!` skips the limiter and
//! belongs to host-only paths: startup, configuration, snapshots. `debug!` and
//! `trace!` pass straight through to `log`.

pub mod rate_limited;

pub use log::{Level, LevelFilter, STATIC_MAX_LEVEL, max_level};
// Re-exported under private names so the wrapper macros below can reach the
// real ones while `clippy.toml` keeps `log::*` off limits everywhere else.
#[doc(hidden)]
pub use log::{
    debug as __debug, error as __error, info as __info, log as __log, trace as __trace,
    warn as __warn,
};

/// Debug log, straight through to `log::debug!`.
#[macro_export]
macro_rules! debug {
    ($($arg:tt)+) => {{
        #[allow(clippy::disallowed_macros)]
        { $crate::__debug!($($arg)+) }
    }};
}

/// Trace log, straight through to `log::trace!`.
#[macro_export]
macro_rules! trace {
    ($($arg:tt)+) => {{
        #[allow(clippy::disallowed_macros)]
        { $crate::__trace!($($arg)+) }
    }};
}

/// Log at a dynamically selected level without rate limiting. Host-only paths.
#[macro_export]
macro_rules! log_unrestricted {
    ($($arg:tt)+) => {{
        #[allow(clippy::disallowed_macros)]
        { $crate::__log!($($arg)+) }
    }};
}

/// Error log that skips rate limiting. Host-only paths.
#[macro_export]
macro_rules! error_unrestricted {
    ($($arg:tt)+) => {{
        #[allow(clippy::disallowed_macros)]
        { $crate::__error!($($arg)+) }
    }};
}

/// Warning log that skips rate limiting. Host-only paths.
#[macro_export]
macro_rules! warn_unrestricted {
    ($($arg:tt)+) => {{
        #[allow(clippy::disallowed_macros)]
        { $crate::__warn!($($arg)+) }
    }};
}

/// Info log that skips rate limiting. Host-only paths.
#[macro_export]
macro_rules! info_unrestricted {
    ($($arg:tt)+) => {{
        #[allow(clippy::disallowed_macros)]
        { $crate::__info!($($arg)+) }
    }};
}

/// The limiter check shared by the rate-limited macros.
///
/// The `static` lives in the macro body, so every expansion site gets its own
/// limiter and flooding one callsite leaves the others alone. Reporting happens
/// here rather than inside the limiter so `file!` and `line!` resolve to the
/// callsite that flooded.
#[doc(hidden)]
#[macro_export]
macro_rules! __rate_limited {
    ($level:expr, $($arg:tt)+) => {{
        let level = $level;
        // Not `log_enabled!`: on top of these two checks it dispatches into
        // `Log::enabled`, which only reads the same level back again.
        if level <= $crate::STATIC_MAX_LEVEL && level <= $crate::max_level() {
            static LIMITER: $crate::rate_limited::DefaultLogRateLimiter =
                $crate::rate_limited::DefaultLogRateLimiter::new();

            #[allow(clippy::disallowed_macros)]
            match LIMITER.check() {
                $crate::rate_limited::Verdict::Emit { suppressed } => {
                    if suppressed > 0 {
                        $crate::warn_unrestricted!(
                            "rate limiting suppressed {suppressed} messages from this callsite"
                        );
                    }
                    $crate::__log!(level, $($arg)+);
                }
                $crate::rate_limited::Verdict::Deny => {}
            }
        }
    }};
}

/// Rate-limited error log: 10 messages per 5 s per callsite.
///
/// Once the callsite is allowed to log again it first reports how many messages
/// the limiter dropped in the meantime.
#[macro_export]
macro_rules! error {
    ($($arg:tt)+) => {
        $crate::__rate_limited!($crate::Level::Error, $($arg)+)
    };
}

/// Rate-limited warning log. Same terms as [`error!`].
#[macro_export]
macro_rules! warn {
    ($($arg:tt)+) => {
        $crate::__rate_limited!($crate::Level::Warn, $($arg)+)
    };
}

/// Rate-limited info log. Same terms as [`error!`].
#[macro_export]
macro_rules! info {
    ($($arg:tt)+) => {
        $crate::__rate_limited!($crate::Level::Info, $($arg)+)
    };
}
