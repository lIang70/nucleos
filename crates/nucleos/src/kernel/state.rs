//! The event log and the checkpoint: what a driver writes, and when.

use super::{Kernel, StateWrite, session};
use crate::adapter::Clock;
use crate::contract::{Event, EventKind};
use crate::error::{Error, Result};
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
    /// Record that the caller compacted history before building a prompt.
    ///
    /// The kernel does not compact and cannot check this claim: the caller assembled the prompt, the
    /// caller replaced a span, and what it must not do is leave the log describing a prompt that
    /// was never sent. The span is checked for sanity — an empty or backwards range is a bug in the
    /// caller, not a compaction — and the summary is stored as given.
    pub async fn compact(
        &self,
        session: &mut session::Session,
        replaced_from: u32,
        replaced_to: u32,
        summary: impl Into<String>,
    ) -> Result<()> {
        if replaced_from >= replaced_to {
            return Err(Error::invalid(format!(
                "a compaction replaces a non-empty span; {replaced_from}..{replaced_to} is empty"
            )));
        }
        self.emit(
            session,
            EventKind::HistoryCompacted {
                replaced_from,
                replaced_to,
                summary: summary.into(),
            },
        )
        .await?;
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
    /// A syscall, and an honest one: an application that drives its own loop needs to say what
    /// happened, and the log is the record.
    pub async fn record(&self, session: &mut session::Session, kind: EventKind) -> Result<()> {
        self.emit(session, kind).await
    }
    pub(crate) async fn emit(&self, session: &mut session::Session, kind: EventKind) -> Result<()> {
        let event = Event {
            seq: session.seq,
            session: session.id.clone(),
            at_unix_millis: self.clock.now_unix_millis(),
            schema_version: Event::SCHEMA_VERSION,
            kind,
            meta: BTreeMap::new(),
        };
        session.seq = session.seq.saturating_add(1);
        self.store.append(&session.id, &[event]).await
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
    pub async fn checkpoint(&self, session: &session::Session) -> Result<()> {
        self.write_checkpoint(session).await
    }
    pub(crate) async fn write_checkpoint(&self, session: &session::Session) -> Result<()> {
        match session.snapshot() {
            Some(snapshot) => self.store.write_snapshot(snapshot).await,
            None => Ok(()),
        }
    }

    pub(crate) fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }
}
