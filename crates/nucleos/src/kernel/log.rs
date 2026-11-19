//! One head per log: the counter and the lock that make an append atomic.
//!
//! `Session` is a *handle*, and a handle is minted per request (`load` builds one for every call). That
//! is fine for the message list and the checkpoint, and it was wrong for the two things that decide the
//! order of the log: a counter and a mutex that `from_snapshot` created fresh for every handle meant one
//! log could have several counters and several locks, which is exactly the arrangement the lock exists to
//! prevent — the second writer reserves a number the first has already used, and the store refuses it as a
//! gap.
//!
//! So the counter and the lock live in one place per `(tenant, session)`, owned by the kernel, and every
//! handle — a request, the process supervisor, the progress writer — resolves to the same `Arc<LogHead>`.
//!
//! The two belong together for a reason worth stating once: a counter alone guarantees the numbers are
//! *unique*, and a lock alone would serialise writers that still renumber each other. Together they
//! guarantee the numbers are *dense and in order*, which is what the store requires and what every resume
//! cursor is built on.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;

/// The head of one session's log.
#[derive(Debug)]
pub(crate) struct LogHead {
    /// The next free sequence number.
    pub(crate) seq: AtomicU64,
    /// Held from "take the next number" to "the event is in the store".
    pub(crate) appending: Mutex<()>,
}

impl LogHead {
    pub(crate) fn new(next_seq: u64) -> Arc<Self> {
        Arc::new(Self {
            seq: AtomicU64::new(next_seq),
            appending: Mutex::new(()),
        })
    }

    /// The next event's sequence number, for a reader that only wants to look.
    pub(crate) fn next(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_head_starts_at_the_given_sequence() {
        let head = LogHead::new(42);
        assert_eq!(head.next(), 42);
    }

    #[test]
    fn the_counter_is_shared_across_clones_of_the_arc() {
        let head = LogHead::new(0);
        let other = Arc::clone(&head);
        head.seq.fetch_add(1, Ordering::SeqCst);
        assert_eq!(other.next(), 1, "the whole point: one counter per log");
    }

    #[test]
    fn the_lock_is_shared_across_clones_of_the_arc() {
        let head = LogHead::new(0);
        let other = Arc::clone(&head);
        // Acquiring through one Arc excludes acquisition through the other. This is what makes two
        // handles of one session unable to interleave a reservation and a write.
        let guard = head.appending.try_lock();
        assert!(guard.is_ok());
        assert!(other.appending.try_lock().is_err());
    }

    #[tokio::test]
    async fn the_lock_is_released_when_the_guard_drops() {
        let head = LogHead::new(0);
        {
            let _guard = head.appending.lock().await;
            assert!(head.appending.try_lock().is_err());
        }
        assert!(head.appending.try_lock().is_ok());
    }
}
