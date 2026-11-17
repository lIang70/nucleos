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
        self.append_batch(std::slice::from_ref(&kind)).await
    }

    /// Append several events in one store call.
    ///
    /// A process reader produces output in 8 KiB reads, and one transaction per read made a chatty
    /// command cost a commit per chunk. The batch is the same guarantee as the single append — the
    /// numbers are reserved and written under the log's lock, in order — at one commit.
    pub(crate) async fn append_batch(&self, kinds: &[EventKind]) -> Result<()> {
        if kinds.is_empty() {
            return Ok(());
        }
        // Reservation and write, atomically with respect to every other writer of this log — the
        // kernel's `emit` included. Without it, stdout and stderr take numbers that reach the store
        // out of order, and the store refuses the second as a gap: the output is silently lost.
        let _appending = self.head.appending.lock().await;
        let mut events = Vec::with_capacity(kinds.len());
        for kind in kinds {
            let seq = self.head.seq.fetch_add(1, Ordering::SeqCst);
            events.push(Event {
                seq,
                session: self.session.id.clone(),
                at_unix_millis: self.clock.now_unix_millis(),
                schema_version: Event::SCHEMA_VERSION,
                kind: kind.clone(),
                meta: BTreeMap::new(),
            });
        }
        self.store.append(&self.session, &events).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::adapter::Store;
    use crate::contract::{SessionState, Snapshot};
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct FixedClock(u64);
    impl Clock for FixedClock {
        fn now_unix_millis(&self) -> u64 {
            self.0
        }
    }

    /// A store that records what it was given, for checking sequence numbers and batching.
    #[derive(Default)]
    struct Recording {
        batches: Mutex<Vec<Vec<Event>>>,
    }

    #[async_trait]
    impl Store for Recording {
        async fn append(&self, _session: &SessionKey, events: &[Event]) -> Result<()> {
            self.batches.lock().unwrap().push(events.to_vec());
            Ok(())
        }
        async fn sessions(
            &self,
            _query: &crate::adapter::SessionQuery,
        ) -> Result<crate::adapter::SessionPage> {
            unimplemented!("not used in writer tests")
        }
        async fn snapshot(&self, _session: &SessionKey) -> Result<Option<Snapshot>> {
            Ok(None)
        }
        async fn write_snapshot(&self, _snapshot: Snapshot) -> Result<()> {
            unimplemented!("not used in writer tests")
        }
        async fn delete(&self, _session: &SessionKey) -> Result<bool> {
            unimplemented!("not used in writer tests")
        }
        async fn events_since(&self, _session: &SessionKey, _seq: u64) -> Result<Vec<Event>> {
            Ok(Vec::new())
        }
        async fn events_page(
            &self,
            _session: &SessionKey,
            _from_seq: u64,
            _limit: usize,
        ) -> Result<Vec<Event>> {
            unimplemented!("not used in writer tests")
        }
        async fn mark_closed(&self, _session: &SessionKey, _at_unix_millis: u64) -> Result<()> {
            unimplemented!("not used in writer tests")
        }
        async fn prune_closed(
            &self,
            _query: &crate::adapter::PruneQuery,
        ) -> Result<crate::adapter::PruneReport> {
            unimplemented!("not used in writer tests")
        }
        async fn events_before(
            &self,
            _session: &SessionKey,
            _before_seq: u64,
            _limit: usize,
        ) -> Result<Vec<Event>> {
            unimplemented!("not used in writer tests")
        }
    }

    fn state_event() -> EventKind {
        EventKind::StateChanged {
            state: SessionState::Open,
            stop: None,
        }
    }

    fn writer() -> (EventWriter, Arc<Recording>) {
        let store = Arc::new(Recording::default());
        let writer = EventWriter::new(
            store.clone() as Arc<dyn Store>,
            SessionKey::new("acme", crate::contract::SessionId::new("s1")),
            super::super::log::LogHead::new(0),
            Arc::new(FixedClock(1000)),
        );
        (writer, store)
    }

    #[tokio::test]
    async fn an_empty_batch_writes_nothing() {
        let (writer, store) = writer();
        writer.append_batch(&[]).await.unwrap();
        assert!(store.batches.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_single_append_is_a_batch_of_one() {
        let (writer, store) = writer();
        writer.append(state_event()).await.unwrap();
        let batches = store.batches.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[0][0].seq, 0);
        assert_eq!(batches[0][0].at_unix_millis, 1000);
    }

    #[tokio::test]
    async fn a_batch_reserves_dense_sequence_numbers_in_one_store_call() {
        let (writer, store) = writer();
        let kinds = vec![state_event(), state_event(), state_event()];
        writer.append_batch(&kinds).await.unwrap();
        let batches = store.batches.lock().unwrap();
        assert_eq!(batches.len(), 1, "one transaction, not three");
        let seqs: Vec<u64> = batches[0].iter().map(|event| event.seq).collect();
        assert_eq!(seqs, [0, 1, 2], "dense and in order");
    }

    #[tokio::test]
    async fn successive_batches_continue_the_sequence() {
        let (writer, store) = writer();
        writer
            .append_batch(&[state_event(), state_event()])
            .await
            .unwrap();
        writer.append_batch(&[state_event()]).await.unwrap();
        let batches = store.batches.lock().unwrap();
        assert_eq!(batches[1][0].seq, 2, "the counter does not reset");
    }

    #[tokio::test]
    async fn the_clock_is_the_injected_one() {
        let (writer, store) = writer();
        writer.append(state_event()).await.unwrap();
        let batches = store.batches.lock().unwrap();
        assert_eq!(batches[0][0].at_unix_millis, 1000);
    }
}
