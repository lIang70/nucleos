//! The event log and the checkpoint: what a driver writes, and when.

use super::{Kernel, StateWrite, session};
use crate::adapter::Clock;
use crate::contract::{Event, EventKind};
use crate::error::Result;
use std::collections::BTreeMap;
use std::sync::Arc;

/// What a checkpoint is for, which decides what a refusal means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Checkpoint {
    /// A cache of the log. The work it describes is already written down, so a refusal loses nothing
    /// a `load` cannot fold back — it only means somebody else's checkpoint is newer.
    Cache,
    /// The only copy. What it describes has no event behind it, so a refusal is a lost write and the
    /// caller must be told.
    Durable,
}

impl Kernel {
    /// Append to the session's conversation or its opaque state.
    ///
    /// The only write with no event behind it: what it carries is durable **because** of its
    /// checkpoint, so a refused checkpoint is a lost write rather than a stale cache and has to
    /// surface as an error.
    pub async fn append(&self, session: &mut session::Session, write: StateWrite) -> Result<()> {
        for (key, value) in write.data {
            session.data.insert(key, value);
        }
        session.messages.extend(write.messages);
        // The driver has consumed what the kernel announced — including any `MessageCompleted` — so the
        // state now reflects those events and a checkpoint may claim them.
        session.materialised_through = match (session.materialised_through, session.written_through)
        {
            (Some(applied), Some(written)) => Some(applied.max(written)),
            (None, written) => written,
            (applied, None) => applied,
        };
        self.checkpoint_with(session, Checkpoint::Durable).await
    }
    /// Park a call on a human.
    pub async fn park(
        &self,
        session: &mut session::Session,
        pending: crate::contract::PendingCall,
    ) -> Result<()> {
        self.emit(
            session,
            EventKind::AwaitingHuman {
                question: pending.question.clone(),
                tool_call_id: pending.call.id.clone(),
                options: pending.options.clone(),
                default_if_unanswered: crate::contract::PermissionKind::Reject,
            },
        )
        .await?;
        session.pending.push(pending);
        // The `AwaitingHuman` event is the narrative record; the pending list itself travels only in
        // the checkpoint, so a refusal means the answer would have nowhere to land.
        self.checkpoint_with(session, Checkpoint::Durable).await
    }
    /// Append an event to the session's log.
    ///
    pub(crate) async fn emit(&self, session: &mut session::Session, kind: EventKind) -> Result<()> {
        // Fragments are the high-frequency events — a streamed answer is thousands of them — and each
        // one used to be a transaction against one connection. They wait in the session until the
        // batch is worth a transaction, or until the stream is slow enough that waiting would be
        // visible: a model emitting ten tokens a second must still appear one token at a time.
        if is_fragment(&kind)
            && session.buffered_events.len() < FRAGMENTS_PER_APPEND
            && session.last_flush.elapsed() < FLUSH_AFTER
        {
            session.buffered_events.push(kind);
            return Ok(());
        }
        // Anything that is not a fragment goes out after the fragments before it, in one order.
        self.flush(session).await?;
        self.append_events(session, vec![kind]).await
    }

    /// Write whatever the turn has buffered.
    ///
    /// Called by `emit` before a non-fragment, and by the turn's ending paths — a buffer left behind is
    /// deltas a reader saw that the store never got.
    pub(crate) async fn flush(&self, session: &mut session::Session) -> Result<()> {
        if session.buffered_events.is_empty() {
            return Ok(());
        }
        let buffered = std::mem::take(&mut session.buffered_events);
        session.last_flush = std::time::Instant::now();
        self.append_events(session, buffered).await
    }

    /// Append events in one write, then announce them in order.
    async fn append_events(
        &self,
        session: &mut session::Session,
        kinds: Vec<EventKind>,
    ) -> Result<()> {
        if kinds.is_empty() {
            return Ok(());
        }
        // Held across reservation *and* append, so this batch cannot be overtaken by a process
        // supervisor writing to the same log. Two writers, one order.
        let _appending = session.log.appending.lock().await;
        let key = session.key();
        let kinds_first = kinds[0].clone();
        let mut events = Vec::with_capacity(kinds.len());
        for kind in kinds {
            let event = Event {
                seq: session.log.seq.load(std::sync::atomic::Ordering::SeqCst),
                session: session.id.clone(),
                at_unix_millis: self.clock.now_unix_millis(),
                schema_version: Event::SCHEMA_VERSION,
                kind,
                meta: BTreeMap::new(),
            };
            session
                .log
                .seq
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            events.push(event);
        }
        // Written first, announced second. The order is the guarantee: by the time a follower hears
        // about an event, a reader of the store can already see it, so catching up from a sequence
        // number never lands short.
        self.store.append(&key, &events).await?;
        // This handle now holds everything up to its own last write. Advanced here, after the store
        // accepted the batch, so a checkpoint can never claim an event that was not written.
        if let Some(last) = events.last() {
            session.written_through = Some(
                session
                    .written_through
                    .map_or(last.seq, |through| through.max(last.seq)),
            );
            // `MessageCompleted` is announced, not applied: the message enters history on the caller's
            // next `state` write, so claiming it here would let a checkpoint describe a conversation
            // its own state does not contain — and the next `load` would skip the message. Everything
            // else this crate emits updates the session before or immediately after the append.
            let announced_only = matches!(kinds_first, EventKind::MessageCompleted { .. });
            if !announced_only {
                session.materialised_through = Some(
                    session
                        .materialised_through
                        .map_or(last.seq, |through| through.max(last.seq)),
                );
            }
        }
        for event in events {
            self.live.announce(super::live::Live {
                key: key.clone(),
                event,
            });
        }
        Ok(())
    }
    /// `record` the session's current state.
    pub async fn record_state(&self, session: &mut session::Session) -> Result<()> {
        self.emit_state(session).await
    }
    pub(crate) async fn emit_state(&self, session: &mut session::Session) -> Result<()> {
        let kind = EventKind::StateChanged {
            state: session.state,
            stop: None,
        };
        self.emit(session, kind).await
    }
    /// Write the checkpoint for the boundary the session is standing on.
    pub async fn checkpoint(&self, session: &mut session::Session) -> Result<()> {
        self.write_checkpoint(session).await
    }

    /// Write a checkpoint only when this handle owes one.
    ///
    /// The turn-success path uses this: the events that describe the turn are already in the log, and
    /// `load` folds them. A checkpoint is a cache, so paying to rewrite the whole snapshot after every
    /// turn is a cost with no answer — except that it bounds how much a crash has to re-fold, which is
    /// what the two thresholds bound instead.
    ///
    /// Paths that *end* something still force one: a close, a park, a configuration change and a
    /// failure each write state the caller is entitled to see durable immediately, and none of them is
    /// on a hot path.
    pub(crate) async fn checkpoint_if_due(&self, session: &mut session::Session) -> Result<()> {
        if session.checkpoint_due() {
            self.write_checkpoint(session).await
        } else {
            Ok(())
        }
    }

    /// Write the checkpoint, and move the version only if the store accepted it.
    ///
    /// A refusal is not a failure. The store rejects anything that is not strictly newer than what it
    /// holds, which is how a **stale handle** is stopped from overwriting a fresher materialisation —
    /// but this handle's own work is already in the log, and the checkpoint is a cache of that log.
    /// Failing the caller here would turn "somebody else checkpointed more recently" into "your close
    /// failed", which is a lie about what was durable.
    pub(crate) async fn write_checkpoint(&self, session: &mut session::Session) -> Result<()> {
        self.checkpoint_with(session, Checkpoint::Cache).await
    }

    /// Write the checkpoint, and decide what a refusal means from the caller's side.
    pub(crate) async fn checkpoint_with(
        &self,
        session: &mut session::Session,
        what: Checkpoint,
    ) -> Result<()> {
        let next = session.version.saturating_add(1);
        match session.checkpoint_draft(self.now(), next) {
            Some(checkpoint) => {
                // What this checkpoint actually includes, taken before it is moved into the store: a
                // supervisor may advance the log while the write is in flight, and the throttle must
                // count the events that are durable, not the ones that exist.
                let through = checkpoint.snapshot.seq;
                let (messages_held, attempts_held) = (
                    checkpoint.messages_from + checkpoint.messages.len(),
                    checkpoint.attempts_from + checkpoint.attempts.len(),
                );
                match self.store.write_checkpoint(checkpoint).await {
                    Ok(()) => {
                        session.version = next;
                        // Only on success: a refused checkpoint wrote nothing, so the events it would
                        // have covered are still the ones a `load` has to fold.
                        session.checkpointed_through = through;
                        session.messages_held = messages_held;
                        session.attempts_held = attempts_held;
                        session.last_checkpoint = std::time::Instant::now();
                        Ok(())
                    }
                    Err(error)
                        if error.code() == crate::contract::ErrorCode::Conflict
                            && what == Checkpoint::Cache =>
                    {
                        tracing::debug!(
                            event = "checkpoint_skipped",
                            session = %session.id,
                            version = next,
                            consequence = "a newer checkpoint exists; the log already has this handle's writes",
                        );
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
            None => Ok(()),
        }
    }

    pub(crate) fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }
}

/// How many events may accumulate past the last checkpoint before one is written anyway.
///
/// It bounds how much a crash has to re-fold, which is the only thing periodic checkpointing buys once
/// the log is the record. 512 is a few turns of a streamed answer: a `load` folds them in about a
/// millisecond, and a checkpoint of a long session is far more expensive than that.
pub(crate) const CHECKPOINT_EVENTS: u64 = 512;

/// ...and how long a checkpoint may be skipped, so a session on a slow trickle still bounds its tail.
pub(crate) const CHECKPOINT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// How many streamed fragments go into one append.
///
/// Sixteen is a compromise measured against the store: a batch of sixteen costs about a quarter of the
/// per-event time of sixteen single writes, and the window it holds open is short enough that a crash
/// loses at most these.
const FRAGMENTS_PER_APPEND: usize = 16;

/// ...and how long a buffer may wait, so a slow stream is still a stream.
pub(crate) const FLUSH_AFTER: std::time::Duration = std::time::Duration::from_millis(8);

/// Whether an event is a streamed fragment: high frequency, and meaningless on its own.
fn is_fragment(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::TextDelta { .. }
            | EventKind::ReasoningDelta { .. }
            | EventKind::ToolCallArgs { .. }
    )
}
