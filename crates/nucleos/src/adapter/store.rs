//! Where events and snapshots live.
//!
//! Three methods, and the two numbers between them have to agree: a snapshot's `seq` is the
//! last event it already includes, so recovery loads the snapshot and asks for everything
//! *after* it.

use crate::contract::{Event, SessionId, Snapshot};
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
