//! Where events and snapshots live.
//!
//! Three methods, and the two numbers between them have to agree: a snapshot's `seq` is the
//! last event it already includes, so recovery loads the snapshot and asks for everything
//! *after* it.

use crate::contract::{Attempt, Event, Message, SessionId, SessionKind, Snapshot};

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

/// A checkpoint to write: the state, and only the rows the store does not have yet.
///
/// `Snapshot` is the *read* shape — everything, reassembled. Writing it as it comes means cloning the
/// whole conversation on every checkpoint and then throwing away all but the last message or two, which
/// is what this type exists to stop: a session hands over the state plus the tail it added, and the
/// tail is what the store binds.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    /// The state. `messages` and `ledger.attempts` are empty; the rows are in the fields below.
    pub snapshot: Snapshot,
    /// Where `messages` starts in the conversation, as an ordinal.
    pub messages_from: usize,
    /// The conversation from `messages_from` onward.
    pub messages: Vec<Message>,
    /// Where `attempts` starts in the ledger, as an ordinal.
    pub attempts_from: usize,
    /// The ledger from `attempts_from` onward.
    pub attempts: Vec<Attempt>,
}

impl Checkpoint {
    /// A checkpoint carrying the whole conversation, for a caller that has no delta to offer.
    ///
    /// What a test builds, and what a supervisor does when it reads a snapshot back, mutates one field
    /// and writes it again. The store still writes only the rows it does not hold, so this costs the
    /// same on disk; it costs a clone in the caller, which is why the kernel does not use it.
    #[must_use]
    pub fn whole(snapshot: Snapshot) -> Self {
        let mut snapshot = snapshot;
        let messages = std::mem::take(&mut snapshot.messages);
        let attempts = std::mem::take(&mut snapshot.ledger.attempts);
        Self {
            snapshot,
            messages_from: 0,
            messages,
            attempts_from: 0,
            attempts,
        }
    }

    /// The checkpoint as a reader would see it.
    #[must_use]
    pub fn into_snapshot(mut self) -> Snapshot {
        self.snapshot.messages = self.messages;
        self.snapshot.ledger.attempts = self.attempts;
        self.snapshot
    }
}

/// What a retention pass should remove.
///
/// Only closed sessions are ever candidates: an open session is somebody's work in progress, and a
/// parked one is waiting on a human, not garbage.
#[derive(Debug, Clone)]
pub struct PruneQuery {
    /// Only this tenant's sessions. Absent means every tenant, which is an operator's question rather
    /// than a caller's.
    pub tenant: Option<String>,
    /// Delete sessions whose last activity is strictly before this, in unix milliseconds.
    pub before_unix_millis: u64,
    /// How many sessions one pass may remove. A bound, so a retention pass is a batch rather than an
    /// unbounded delete on a live store.
    pub limit: usize,
    /// Report what would go without removing it. The default for any manual call.
    pub dry_run: bool,
}

/// What a retention pass did, or would do.
#[derive(Debug, Clone, Default)]
pub struct PruneReport {
    /// How many sessions matched.
    pub sessions: u64,
    /// How many events they held.
    pub events: u64,
    /// Which sessions, so the caller can forget any in-memory heads and an operator can see what went.
    pub candidates: Vec<SessionKey>,
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
    ///
    /// Reassembled: the state, the conversation and the ledger may live in different rows, and a caller
    /// gets the session, not the storage layout.
    async fn snapshot(&self, session: &SessionKey) -> Result<Option<Snapshot>>;

    /// The same checkpoint, without the parts that grow with a session's age.
    ///
    /// `messages` and `ledger` are empty: a view needs what the session *is* — its state, its spend,
    /// the call it waits on — and renders neither of those, while they are the whole cost of parsing a
    /// long session. A view is the endpoint clients poll.
    ///
    /// The default implementation is for stores that do not keep a second copy: correct, just not
    /// faster. `nucleos-sqlite` keeps one, produced from the body at write time.
    async fn snapshot_head(&self, session: &SessionKey) -> Result<Option<Snapshot>> {
        Ok(self.snapshot(session).await?.map(|mut snapshot| {
            snapshot.messages = Vec::new();
            snapshot.ledger = Default::default();
            snapshot
        }))
    }

    /// Record a checkpoint.
    ///
    /// Part of the trait rather than of one backend: writing a snapshot is the kernel's
    /// decision, and a store that cannot accept one makes "checkpoint only at a turn
    /// boundary" unimplementable.
    ///
    /// The snapshot carries the whole session, and a store may write only what changed: it already
    /// holds a prefix of `messages` and of `ledger.attempts`, and the tail is what this call adds.
    async fn write_snapshot(&self, snapshot: Snapshot) -> Result<()>;

    /// Write a checkpoint from a state and its tail.
    ///
    /// The default reassembles the whole snapshot and delegates, which is correct and no faster than
    /// the call it replaces. A store that keeps the conversation as rows overrides it, so the session
    /// can hand over two messages instead of cloning the conversation.
    async fn write_checkpoint(&self, checkpoint: Checkpoint) -> Result<()> {
        self.write_snapshot(checkpoint.into_snapshot()).await
    }

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

    /// A bounded page, for a reader that must not materialise a whole history.
    ///
    /// Streaming a session's events is a loop over this, not one `events_since` and a `Vec` sized by
    /// the session's age: the first frame has to arrive before the last piece of history is read.
    async fn events_page(
        &self,
        session: &SessionKey,
        from_seq: u64,
        limit: usize,
    ) -> Result<Vec<Event>>;

    /// Record that a session is closed, without writing a whole checkpoint.
    ///
    /// The `closed` flag is what `DELETE` and retention consult, and it used to travel only inside a
    /// snapshot. A checkpoint refused as stale therefore left a session the store still believed was
    /// open — unremovable, unprunable, forever, because `close` is idempotent and never retries. This
    /// is the same fact written on its own, where a stale version cannot take it away.
    async fn mark_closed(&self, session: &SessionKey, at_unix_millis: u64) -> Result<()>;

    /// Remove closed sessions whose last activity is before a deadline.
    ///
    /// Whole sessions, never a span of a log: `seq` stays dense for every session that survives, and a
    /// survivor's resume cursor keeps working. A session whose last activity is unknown is kept —
    /// retention may delete what it can date, never what it cannot.
    async fn prune_closed(&self, query: &PruneQuery) -> Result<PruneReport>;

    /// A bounded page read **backwards**, oldest first within the page.
    ///
    /// What reconstructing an unfinished turn needs: the fragment is the tail of the log, so reading
    /// from the end until a boundary costs a turn, not a session.
    async fn events_before(
        &self,
        session: &SessionKey,
        before_seq: u64,
        limit: usize,
    ) -> Result<Vec<Event>>;
}
