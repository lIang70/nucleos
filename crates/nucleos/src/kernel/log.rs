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
