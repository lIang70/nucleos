//! Where events and snapshots live.
//!
//! Three methods, and the two numbers between them have to agree: a snapshot's `seq` is the
//! last event it already includes, so recovery loads the snapshot and asks for everything
//! *after* it.

use crate::contract::{Event, SessionId, SessionKind, Snapshot};

/// The half of a listing a store can answer.
#[derive(Debug, Clone, Default)]
pub struct SessionQuery {
    /// Only sessions of this kind.
    pub kind: Option<SessionKind>,
    /// How many to return.
    pub limit: usize,
    /// Continue after this id. Absent starts at the beginning.
    pub after: Option<SessionId>,
}

/// One page, and where the next one starts.
#[derive(Debug, Clone, Default)]
pub struct SessionPage {
    /// The ids, in order.
    pub sessions: Vec<SessionId>,
    /// Where to continue. Absent means the store had nothing more.
    pub next: Option<SessionId>,
}
use crate::error::Result;
use async_trait::async_trait;

/// The event log and its materialised states.
#[async_trait]
pub trait Store: Send + Sync {
    /// Append events to a session.
    ///
    /// The caller owns the sequence: it passes events whose `seq` values continue exactly
    /// from the current end. A store rejects a gap rather than filling it, because a
    /// silently renumbered event makes every later resume cursor wrong.
    async fn append(&self, session: &SessionId, events: &[Event]) -> Result<()>;

    /// A page of session ids, oldest-first by id.
    ///
    /// Only the store knows what exists; a supervisor knows what is *running*, which is a different
    /// and smaller set. The store answers the cheap half of a listing — filter by kind, order, page
    /// — because a real store can push that into an index and this one cannot.
    async fn sessions(&self, query: &SessionQuery) -> Result<SessionPage>;

    /// The most recent snapshot, if one was ever written.
    async fn snapshot(&self, session: &SessionId) -> Result<Option<Snapshot>>;

    /// Record a checkpoint.
    ///
    /// Part of the trait rather than of one backend: writing a snapshot is the kernel's
    /// decision, and a store that cannot accept one makes "checkpoint only at a turn
    /// boundary" unimplementable.
    async fn write_snapshot(&self, snapshot: Snapshot) -> Result<()>;

    /// Events from `seq` onward, inclusive, oldest first.
    ///
    /// Inclusive is the only choice that makes `from_seq` in the ABI mean what it says: a
    /// client asking from `seq` wants that event, not the one after it.
    async fn events_since(&self, session: &SessionId, seq: u64) -> Result<Vec<Event>>;
}
