//! The event log and the checkpoint: what a driver writes, and when.

use super::{Kernel, StateWrite, session};
use crate::adapter::Clock;
use crate::contract::{Event, EventKind};
use crate::error::Result;
use std::collections::BTreeMap;
use std::sync::Arc;

impl Kernel {
    /// Append to the session's conversation or its opaque state.
    pub async fn append(&self, session: &mut session::Session, write: StateWrite) -> Result<()> {
        for (key, value) in write.data {
            session.data.insert(key, value);
        }
        session.messages.extend(write.messages);
        self.write_checkpoint(session).await
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
        self.write_checkpoint(session).await
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
    /// Write the checkpoint for the boundary the session is standing on.
    pub async fn checkpoint(&self, session: &mut session::Session) -> Result<()> {
        self.write_checkpoint(session).await
    }

    /// Write the checkpoint, and move the version only if the store accepted it.
    ///
    /// A refusal is not a failure. The store rejects anything that is not strictly newer than what it
    /// holds, which is how a **stale handle** is stopped from overwriting a fresher materialisation —
    /// but this handle's own work is already in the log, and the checkpoint is a cache of that log.
    /// Failing the caller here would turn "somebody else checkpointed more recently" into "your close
    /// failed", which is a lie about what was durable.
    pub(crate) async fn write_checkpoint(&self, session: &mut session::Session) -> Result<()> {
        let next = session.version.saturating_add(1);
        match session.snapshot(self.now(), next) {
            Some(snapshot) => match self.store.write_snapshot(snapshot).await {
                Ok(()) => {
                    session.version = next;
                    Ok(())
                }
                Err(error) if error.code() == crate::contract::ErrorCode::Conflict => {
                    tracing::debug!(
                        event = "checkpoint_skipped",
                        session = %session.id,
                        version = next,
                        consequence = "a newer checkpoint exists; the log is the truth and has this                                        handle's writes",
                    );
                    Ok(())
                }
                Err(error) => Err(error),
            },
            None => Ok(()),
        }
    }

    pub(crate) fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }
}

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
