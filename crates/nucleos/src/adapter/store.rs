//! Where events and snapshots live.
//!
//! Three methods, and the two numbers between them have to agree: a snapshot's `seq` is the
//! last event it already includes, so recovery loads the snapshot and asks for everything
//! *after* it.

use crate::contract::{Event, SessionId, SessionKind, Snapshot};

/// Which session, in whose tenant.
///
/// The store's key is the pair, not the id: session identifiers are chosen by callers, and two
/// tenants each choosing "s1" is ordinary rather than adversarial. Making the tenant part of the key
/// means a lookup cannot accidentally span tenants — the isolation is the schema's, not a check
/// somebody has to remember to write.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionKey {
    /// The tenant that owns it.
    pub tenant: String,
    /// The caller-chosen identifier, unique within the tenant.
    pub id: SessionId,
}

impl SessionKey {
    /// The key for one tenant's session.
    #[must_use]
    pub fn new(tenant: impl Into<String>, id: impl Into<SessionId>) -> Self {
        Self {
            tenant: tenant.into(),
            id: id.into(),
        }
    }
}

impl std::fmt::Display for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.tenant, self.id)
    }
}

/// The half of a listing a store can answer.
#[derive(Debug, Clone, Default)]
pub struct SessionQuery {
    /// Only this tenant's sessions. The listing is scoped for the same reason the lookups are.
    pub tenant: String,
    /// Only this subject's sessions within the tenant.
    ///
    /// A filter in the store rather than a filter afterwards: a page that is narrowed by the caller
    /// after the store has already chosen it can come back short *and* with a cursor, which tells a
    /// client both "there are more" and "you have seen them all".
    pub subject: Option<String>,
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
    async fn append(&self, session: &SessionKey, events: &[Event]) -> Result<()>;

    /// A page of session ids, oldest-first by id.
    ///
    /// Only the store knows what exists; a supervisor knows what is *running*, which is a different
    /// and smaller set. The store answers the cheap half of a listing — filter by kind, order, page
    /// — because a real store can push that into an index and this one cannot.
    async fn sessions(&self, query: &SessionQuery) -> Result<SessionPage>;

    /// The most recent snapshot, if one was ever written.
    async fn snapshot(&self, session: &SessionKey) -> Result<Option<Snapshot>>;

    /// Record a checkpoint.
    ///
    /// Part of the trait rather than of one backend: writing a snapshot is the kernel's
    /// decision, and a store that cannot accept one makes "checkpoint only at a turn
    /// boundary" unimplementable.
    async fn write_snapshot(&self, snapshot: Snapshot) -> Result<()>;

    /// Remove one session: its events, its checkpoint and its listing row.
    ///
    /// One call rather than three deletions at the call site, because a half-deleted session is a
    /// listing entry with no log — which reads as a session that lost its history rather than one that
    /// was removed. Returns whether anything was there.
    ///
    /// Retention is the only caller today. The log grows forever otherwise, and `config.md` promises
    /// pruning, so the primitive exists before the policy that drives it.
    async fn delete(&self, session: &SessionKey) -> Result<bool>;

    /// Events from `seq` onward, inclusive, oldest first.
    ///
    /// Inclusive is the only choice that makes `from_seq` in the ABI mean what it says: a
    /// client asking from `seq` wants that event, not the one after it.
    async fn events_since(&self, session: &SessionKey, seq: u64) -> Result<Vec<Event>>;
}
