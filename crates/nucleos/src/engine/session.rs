//! The session: one state machine, one writer, one checkpoint at a time.
//!
//! The state machine is the reason the engine can be simple. Transitions are checked, terminal
//! states are terminal, and a checkpoint is only ever written when the machine is at a
//! boundary — so "resume" never has to reason about a half-applied turn.

use crate::contract::{
    Budget, Generation, Ledger, Message, PendingCall, Principal, SessionId, SessionState, Snapshot,
    Usage,
};
use crate::error::{Error, Result};

/// A session in flight.
///
/// Owned by exactly one task for its whole life. Nothing hands out `&mut` to it, which is what
/// makes checkpointing unambiguous: the writer knows when the state is settled because it *is*
/// the only writer, and it is standing at a turn boundary.
#[derive(Debug)]
pub(crate) struct Session {
    pub id: SessionId,
    /// Who is asking. Carried for every policy gate, and never derived from a request field.
    pub principal: Principal,
    pub generation: Generation,
    pub state: SessionState,
    pub messages: Vec<Message>,
    pub usage: Usage,
    pub steps: u64,
    pub ledger: Ledger,
    /// Tokens the caller has granted and not yet spent.
    pub credit_tokens: u64,
    pub budget: Budget,
    pub started_unix_millis: u64,
    /// The next event's sequence number.
    pub seq: u64,
    /// The call this session is parked on, when it is parked.
    pub pending: Option<PendingCall>,
    /// Extra attempts per route target, resolved when the session started.
    pub retries: u32,
    /// What the caller asked for, kept for building each request.
    pub params: crate::adapter::GenerateParams,
    /// Standing instructions.
    pub system: Option<String>,
    /// Tool names this session may use. Empty means every registered tool.
    pub tools: Vec<String>,
}

impl Session {
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
            state: SessionState::Created,
            messages: Vec::new(),
            usage: Usage::default(),
            steps: 0,
            ledger: Ledger::default(),
            credit_tokens,
            budget,
            started_unix_millis: now_unix_millis,
            seq: 0,
            pending: None,
            retries: 0,
            params: crate::adapter::GenerateParams::default(),
            system: None,
            tools: Vec::new(),
        }
    }

    /// Move to a state, if the move is legal.
    ///
    /// Returns `false` when nothing changed, so the caller only emits an event on a real
    /// transition. A state machine that reports transitions it did not make is worse than none.
    pub(crate) fn transition(&mut self, to: SessionState) -> Result<bool> {
        if self.state == to {
            return Ok(false);
        }
        if self.state.is_terminal() {
            return Err(Error::internal(format!(
                "session {} is {:?} and cannot move to {to:?}",
                self.id, self.state
            )));
        }
        if !legal(self.state, to) {
            return Err(Error::internal(format!(
                "illegal transition {:?} -> {to:?}",
                self.state
            )));
        }
        self.state = to;
        Ok(true)
    }

    /// Whether the session has run out of room in any dimension.
    ///
    /// Checked before a turn rather than during one: a turn that starts with no room left is a
    /// turn whose output has to be thrown away.
    pub(crate) fn exhausted(&self, now_unix_millis: u64) -> Option<crate::contract::StopReason> {
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
            steps: self.steps,
            pending: self.pending.clone(),
        })
    }

    /// Rebuild a session from a checkpoint and the spec the caller re-sent.
    pub(crate) fn from_snapshot(
        snapshot: Snapshot,
        principal: Principal,
        budget: Budget,
        credit_tokens: u64,
        now_unix_millis: u64,
    ) -> Self {
        Self {
            id: snapshot.session,
            principal,
            generation: snapshot.generation,
            state: snapshot.state,
            messages: snapshot.messages,
            usage: snapshot.usage,
            steps: snapshot.steps,
            ledger: Ledger::default(),
            credit_tokens,
            budget,
            // A resumed session's wall clock starts now: the time a paused session spent
            // waiting for a human is not the model's latency, and charging it would make every
            // wall-clock budget meaningless the first time somebody thought about it.
            started_unix_millis: now_unix_millis,
            // The next event continues the log; the snapshot's `seq` is the last one included.
            seq: snapshot.seq.saturating_add(1),
            pending: snapshot.pending,
            retries: 0,
            params: crate::adapter::GenerateParams::default(),
            system: None,
            tools: Vec::new(),
        }
    }
}

/// Which transitions exist.
fn legal(from: SessionState, to: SessionState) -> bool {
    use SessionState::{
        AwaitingHuman, AwaitingTool, Cancelled, Cancelling, Created, Done, Failed, Running,
    };
    matches!(
        (from, to),
        (Created, Running)
            | (Created, Cancelled)
            | (Running, Running)
            | (Running, AwaitingTool)
            | (Running, AwaitingHuman)
            | (Running, Cancelling)
            | (Running, Done)
            | (Running, Failed)
            | (AwaitingTool, Running)
            | (AwaitingTool, AwaitingHuman)
            | (AwaitingTool, Failed)
            | (AwaitingHuman, Running)
            | (AwaitingHuman, Cancelling)
            | (Cancelling, Done)
            | (Cancelling, Cancelled)
            | (Cancelling, Failed)
    )
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
    fn a_terminal_state_is_terminal() {
        let mut session = session();
        session.transition(SessionState::Running).unwrap();
        session.transition(SessionState::Done).unwrap();
        let error = session.transition(SessionState::Running).unwrap_err();
        assert!(error.reason().contains("cannot move"));
    }

    #[test]
    fn a_skipped_transition_is_refused() {
        let mut session = session();
        let error = session.transition(SessionState::Done).unwrap_err();
        assert!(error.reason().contains("illegal transition"));
    }

    #[test]
    fn a_checkpoint_before_any_event_is_not_written() {
        // A snapshot that includes no event would claim a seq the log does not have.
        assert!(session().snapshot().is_none());
    }
}
