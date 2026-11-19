//! Time, as an input.
//!
//! The kernel never reads the clock directly. Every deadline and every wall-clock budget
//! consults this trait, which is what makes a budget test a pure function and a replayed
//! session deterministic.
//!
//! It reports **unix milliseconds**, not [`std::time::Instant`]: events carry unix
//! milliseconds, budgets are expressed in milliseconds, and `Instant` can neither be
//! serialised nor reconstructed, so a replay could not reproduce it.

use std::time::{SystemTime, UNIX_EPOCH};

/// What time it is.
pub trait Clock: Send + Sync {
    /// Milliseconds since the unix epoch.
    fn now_unix_millis(&self) -> u64;
}

/// The real clock.
///
/// A clock before 1970 returns 0 rather than panicking: a kernel that aborts because a
/// machine's clock is wrong turns a test environment into an outage.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_millis(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_clock_is_after_2020() {
        // 2020-01-01 in unix millis. A clock before this is a machine whose clock is wrong, and the
        // value should still be reported rather than panicking.
        const YEAR_2020: u64 = 1_577_836_800_000;
        let now = SystemClock.now_unix_millis();
        assert!(now > YEAR_2020, "got {now}");
    }

    #[test]
    fn the_system_clock_never_goes_backwards_between_two_reads() {
        let first = SystemClock.now_unix_millis();
        let second = SystemClock.now_unix_millis();
        assert!(second >= first);
    }
}
