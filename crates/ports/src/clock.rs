//! What time it is, as an input rather than an ambient fact.
//!
//! Facts carry unix milliseconds, and `Instant` can neither be serialised nor
//! reconstructed — so a replay could not reproduce one. Time is therefore a value
//! the host supplies, which is also what lets a test move it.

/// A clock.
pub trait Clock: Send + Sync {
    /// Now, in unix milliseconds.
    fn now_unix_millis(&self) -> u64;
}

/// The wall clock. The one implementation a deployment always wants, because
/// reading the time is not a decision.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_millis(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_clock_is_past_the_epoch() {
        assert!(SystemClock.now_unix_millis() > 1_700_000_000_000);
    }

    #[test]
    fn the_system_clock_does_not_go_backwards_between_two_reads() {
        let clock = SystemClock;
        let first = clock.now_unix_millis();
        let second = clock.now_unix_millis();
        assert!(second >= first);
    }
}
