//! Appending events from a task that does not own the session.
//!
//! Two kinds of work run beside the session rather than inside it: a supervised process, and a tool
//! call that reports progress while it runs. Both append to the same log, and both need the same
//! three things — the store, the session id, and the sequence counter the session shares with them.
//!
//! The counter is the `Arc<AtomicU64>` the session itself uses, never a copy of its value. Two
//! counters counting the same events disagree the moment one of them writes, and the store refuses
//! the second one as a sequence gap.

use crate::adapter::Clock;
use crate::adapter::SessionKey;
use crate::contract::{Event, EventKind};
use crate::error::Result;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// A session's log, as a task sees it.
#[derive(Clone)]
pub(crate) struct EventWriter {
    pub(crate) store: Arc<dyn crate::adapter::Store>,
    pub(crate) session: SessionKey,
    /// The log's own counter and lock, shared with the session and every other writer of it.
    pub(crate) head: Arc<super::log::LogHead>,
    clock: Arc<dyn Clock>,
}

impl EventWriter {
    pub(crate) fn new(
        store: Arc<dyn crate::adapter::Store>,
        session: SessionKey,
        head: Arc<super::log::LogHead>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            session,
            head,
            clock,
        }
    }

    pub(crate) async fn append(&self, kind: EventKind) -> Result<()> {
        // Reservation and write, atomically with respect to every other writer of this log — the
        // kernel's `emit` included. Without it, stdout and stderr take numbers that reach the store
        // out of order, and the store refuses the second as a gap: the output is silently lost.
        let _appending = self.head.appending.lock().await;
        let seq = self.head.seq.fetch_add(1, Ordering::SeqCst);
        let event = Event {
            seq,
            session: self.session.id.clone(),
            at_unix_millis: self.clock.now_unix_millis(),
            schema_version: Event::SCHEMA_VERSION,
            kind,
            meta: BTreeMap::new(),
        };
        self.store.append(&self.session, &[event]).await
    }
}
