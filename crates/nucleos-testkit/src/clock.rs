//! A clock that only moves when told to.

use nucleos::adapter::Clock;
use std::sync::{Arc, Mutex};

/// A clock that returns the time it was set to, and moves only when advanced.
#[derive(Debug, Clone, Default)]
pub struct FixedClock(Arc<Mutex<u64>>);

impl FixedClock {
    /// Start at a given unix millisecond.
    #[must_use]
    pub fn at(millis: u64) -> Self {
        Self(Arc::new(Mutex::new(millis)))
    }

    /// Move forward.
    pub fn advance(&self, millis: u64) {
        let mut now = self.lock();
        *now = now.saturating_add(millis);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, u64> {
        // A poisoned lock means another thread panicked while holding it. Recovering is right
        // for a fake: the value is a plain integer with no invariant to preserve.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Clock for FixedClock {
    fn now_unix_millis(&self) -> u64 {
        *self.lock()
    }
}
