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
