//! Cancelling a turn that is already running.
//!
//! A budget bounds a turn and a deadline bounds a session; neither stops one *now*. An operator who
//! sees a model looping, or a caller who changed its mind, needs a way to end the current attempt
//! without closing the session — closing it would throw away the conversation to stop one call.
//!
//! The mechanism is a flag, a notification, and an ordering rule:
//!
//! ```text
//! let waiting = notified();      // register interest *first*
//! if flag { return; }            // then check — otherwise a cancel between the two is lost
//! waiting.await
//! ```
//!
//! Registering before checking is the whole trick. The other order drops a cancellation that arrives
//! in the gap, and a turn that ignores a cancel is worse than one that cannot be cancelled: the caller
//! believes it stopped.
//!
//! What cancelling does *not* do: it does not retry, does not close the session, and does not remove
//! what the attempt already produced. The spend is kept and the attempt is recorded as failed with
//! `Cancelled`, because a turn that streamed half an answer and then stopped is a fact about the bill
//! and about the log.

use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

/// A one-shot cancellation signal, shared between the caller and the turn it is cancelling.
#[derive(Debug, Default)]
pub struct Cancel {
    cancelled: AtomicBool,
    notify: Notify,
}

impl Cancel {
    /// A signal nobody has fired.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fire it. Idempotent, and safe to call from any thread.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Whether it has been fired.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Wait until it is fired, or return immediately if it already has been.
    pub async fn cancelled(&self) {
        // Register before checking. The other order loses a cancellation that lands between the
        // load and the await, and a turn that ignores a cancel is worse than one that cannot be
        // cancelled — the caller believes it stopped.
        let waiting = self.notify.notified();
        if self.is_cancelled() {
            return;
        }
        waiting.await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn cancelling_before_the_wait_returns_immediately() {
        let cancel = Cancel::new();
        cancel.cancel();
        // Would hang for the duration of the timeout if the ordering were wrong.
        tokio::time::timeout(Duration::from_millis(50), cancel.cancelled())
            .await
            .expect("an already-cancelled signal returns at once");
    }

    #[tokio::test]
    async fn cancelling_during_the_wait_wakes_it() {
        let cancel = Arc::new(Cancel::new());
        let waiter = {
            let cancel = Arc::clone(&cancel);
            tokio::spawn(async move { cancel.cancelled().await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_millis(100), waiter)
            .await
            .expect("the waiter wakes")
            .unwrap();
    }

    #[tokio::test]
    async fn a_signal_that_is_not_fired_does_not_wake_anyone() {
        let cancel = Cancel::new();
        assert!(!cancel.is_cancelled());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), cancel.cancelled())
                .await
                .is_err(),
            "nothing happened, so nothing returned"
        );
    }

    #[tokio::test]
    async fn firing_twice_is_harmless() {
        let cancel = Cancel::new();
        cancel.cancel();
        cancel.cancel();
        assert!(cancel.is_cancelled());
    }
}
