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
        // Held across the append, so this reservation cannot be overtaken by a process supervisor
        // writing to the same log. Two writers, one order.
        let _appending = session.log.appending.lock().await;
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
        let key = session.key();
        // Written first, announced second. The order is the guarantee: by the time a follower hears
        // about an event, a reader of the store can already see it, so catching up from a sequence
        // number never lands short.
        self.store
            .append(&key, std::slice::from_ref(&event))
            .await?;
        self.live.announce(super::live::Live { key, event });
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
