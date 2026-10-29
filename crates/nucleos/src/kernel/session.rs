//! The session: one writer, one checkpoint at a time.
//!
//! The session is the reason the kernel can be simple. It is one object with two states, and a
//! checkpoint is only ever written at a boundary the caller chose — so "resume" never has to
//! reason about a half-applied call.

use crate::contract::{
    Budget, Generation, Ledger, Message, PendingCall, Principal, SessionId, SessionState, Snapshot,
    Usage,
};
use crate::error::Result;

/// A session in flight.
///
/// Owned by exactly one task for its whole life. Nothing hands out `&mut` to it, which is what
/// makes checkpointing unambiguous: the writer knows when the state is settled because it *is*
/// the only writer, and it is standing at a turn boundary.
#[derive(Debug)]
pub struct Session {
    pub(crate) id: SessionId,
    /// Who is asking. Carried for every policy gate, and never derived from a request field.
    pub(crate) principal: Principal,
    pub(crate) generation: Generation,
    pub(crate) state: SessionState,
    pub(crate) messages: Vec<Message>,
    pub(crate) usage: Usage,
    pub(crate) steps: u64,
    pub(crate) ledger: Ledger,
    /// Tokens the caller has granted and not yet spent.
    pub(crate) credit_tokens: u64,
    pub(crate) budget: Budget,
    pub(crate) started_unix_millis: u64,
    /// The next event's sequence number.
    pub(crate) seq: u64,
    /// Calls waiting on a human. A batch can have several.
    pub(crate) pending: Vec<PendingCall>,
    /// The application's own state, opaque to the kernel.
    pub(crate) data: std::collections::BTreeMap<String, serde_json::Value>,
    /// Where commands run, and the default writable root.
    pub(crate) workspace: Option<String>,
    /// What commands may touch.
    pub(crate) sandbox: crate::contract::SandboxPolicy,
    /// What this session is for.
    pub(crate) kind: crate::contract::SessionKind,
    /// The process, when this is a process session.
    pub(crate) process: Option<crate::contract::ProcessState>,
    /// The route this session was started with.
    pub(crate) routing: crate::contract::Routing,
    /// Extra attempts per route target, resolved when the session started.
    pub(crate) retries: u32,
    /// What the caller asked for, kept for building each request.
    pub(crate) params: crate::adapter::GenerateParams,
    /// Standing instructions.
    pub(crate) system: Option<String>,
    /// Tool names this session may use. Empty means every registered tool.
    pub(crate) tools: Vec<String>,
}

impl Session {
    /// The identifier.
    #[must_use]
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// The conversation as the kernel has it.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Calls waiting on a human.
    #[must_use]
    pub fn pending(&self) -> &[PendingCall] {
        &self.pending
    }

    /// Whether the session takes further calls.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state.is_closed()
    }

    /// The application's own state.
    #[must_use]
    pub fn data(&self) -> &std::collections::BTreeMap<String, serde_json::Value> {
        &self.data
    }

    /// What this session is for.
    #[must_use]
    pub fn kind(&self) -> crate::contract::SessionKind {
        self.kind
    }

    /// The process, when this is a process session.
    #[must_use]
    pub fn process(&self) -> Option<&crate::contract::ProcessState> {
        self.process.as_ref()
    }

    /// What this session's commands may touch.
    #[must_use]
    pub fn sandbox(&self) -> &crate::contract::SandboxPolicy {
        &self.sandbox
    }

    /// Where they run.
    #[must_use]
    pub fn workspace(&self) -> Option<&String> {
        self.workspace.as_ref()
    }

    /// The route in force.
    #[must_use]
    pub fn routing(&self) -> &crate::contract::Routing {
        &self.routing
    }

    /// How many inference calls have been made.
    #[must_use]
    pub fn inference_calls(&self) -> u64 {
        self.steps
    }

    /// What it has spent.
    #[must_use]
    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    /// Every attempt, in order, with the sum.
    #[must_use]
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// The next event sequence number.
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.seq
    }

    pub(crate) fn new(
        id: SessionId,
        principal: Principal,
        generation: Generation,
        budget: Budget,
        credit_tokens: u64,
        now_unix_millis: u64,
    ) -> Self {
        Self {
            id,
            principal,
            generation,
            state: SessionState::Open,
            messages: Vec::new(),
            usage: Usage::default(),
            steps: 0,
            ledger: Ledger::default(),
            credit_tokens,
            budget,
            started_unix_millis: now_unix_millis,
            seq: 0,
            pending: Vec::new(),
            data: std::collections::BTreeMap::new(),
            workspace: None,
            sandbox: crate::contract::SandboxPolicy::default(),
            kind: crate::contract::SessionKind::Agent,
            process: None,
            routing: crate::contract::Routing::default(),
            retries: 0,
            params: crate::adapter::GenerateParams::default(),
            system: None,
            tools: Vec::new(),
        }
    }

    /// Close the session. Calls after this are refused.
    ///
    /// There is no transition table any more: with the loop in the application, the kernel knows
    /// two facts about a session — it takes calls, or it does not. The eight states that used to
    /// live here were the loop's vocabulary, and the kernel no longer runs the loop.
    pub(crate) fn close(&mut self) -> Result<bool> {
        if self.state == SessionState::Closed {
            return Ok(false);
        }
        self.state = SessionState::Closed;
        Ok(true)
    }

    /// Whether the session has run out of room in any dimension.
    ///
    /// Checked before a turn rather than during one: a turn that starts with no room left is a
    /// turn whose output has to be thrown away.
    pub fn exhausted(&self, now_unix_millis: u64) -> Option<crate::contract::StopReason> {
        use crate::contract::StopReason;
        if let Some(max) = self.budget.max_steps
            && self.steps >= max
        {
            return Some(StopReason::Steps);
        }
        if let Some(max) = self.budget.max_tokens
            && self.usage.total_tokens() >= max
        {
            return Some(StopReason::Tokens);
        }
        if self.credit_tokens == 0 {
            return Some(StopReason::Tokens);
        }
        if let Some(max) = self.budget.max_micros_usd
            && self.usage.micros_usd.is_some_and(|spent| spent >= max)
        {
            return Some(StopReason::Cost);
        }
        if let Some(max) = self.budget.max_wall_millis
            && now_unix_millis.saturating_sub(self.started_unix_millis) >= max
        {
            return Some(StopReason::Wall);
        }
        None
    }

    /// The checkpoint for the boundary the session is standing on.
    ///
    /// `None` before anything has been recorded: a snapshot that includes no event would claim
    /// a `seq` the log does not have.
    pub(crate) fn snapshot(&self) -> Option<Snapshot> {
        if self.seq == 0 {
            return None;
        }
        Some(Snapshot {
            session: self.id.clone(),
            seq: self.seq - 1,
            generation: self.generation,
            state: self.state,
            messages: self.messages.clone(),
            usage: self.usage.clone(),
            inference_calls: self.steps,
            credit_tokens: self.credit_tokens,
            pending: self.pending.clone(),
            routing: self.routing.clone(),
            system: self.system.clone(),
            tools: self.tools.clone(),
            params: self.params.clone(),
            workspace: self.workspace.clone(),
            sandbox: self.sandbox.clone(),
            kind: self.kind,
            process: self.process.clone(),
            data: self.data.clone(),
        })
    }

    /// Rebuild a session from a checkpoint and the spec the caller re-sent.
    pub(crate) fn from_snapshot(
        snapshot: Snapshot,
        principal: Principal,
        budget: Budget,
        now_unix_millis: u64,
    ) -> Self {
        Self {
            id: snapshot.session,
            principal,
            generation: snapshot.generation,
            state: snapshot.state,
            messages: snapshot.messages,
            usage: snapshot.usage,
            steps: snapshot.inference_calls,
            credit_tokens: snapshot.credit_tokens,
            ledger: Ledger::default(),
            budget,
            // A resumed session's wall clock starts now: the time a paused session spent
            // waiting for a human is not the model's latency, and charging it would make every
            // wall-clock budget meaningless the first time somebody thought about it.
            started_unix_millis: now_unix_millis,
            // The next event continues the log; the snapshot's `seq` is the last one included.
            seq: snapshot.seq.saturating_add(1),
            pending: snapshot.pending,
            data: snapshot.data,
            routing: snapshot.routing,
            retries: 0,
            params: snapshot.params,
            workspace: snapshot.workspace,
            sandbox: snapshot.sandbox,
            kind: snapshot.kind,
            process: snapshot.process,
            system: snapshot.system,
            tools: snapshot.tools,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session::new(
            SessionId::new("s1"),
            Principal {
                tenant: "acme".to_owned(),
                subject: "alice".to_owned(),
            },
            Generation(1),
            Budget::default(),
            100,
            0,
        )
    }

    #[test]
    fn closing_is_idempotent_and_final() {
        let mut session = session();
        assert!(session.close().unwrap(), "the first close is a change");
        assert!(!session.close().unwrap(), "the second is not");
        assert!(session.state.is_closed());
    }

    #[test]
    fn a_checkpoint_before_any_event_is_not_written() {
        // A snapshot that includes no event would claim a seq the log does not have.
        assert!(session().snapshot().is_none());
    }
}
