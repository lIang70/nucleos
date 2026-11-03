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
use std::sync::Arc;

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
    /// The tools this session's calls are answered from.
    ///
    /// Held, not looked up by number. A number in a map is a pin that only holds while the map keeps
    /// the entry; an `Arc` is a pin that holds because somebody is holding it. Retirement is then
    /// just "forget the number", and no session can ever reach for a set that has been freed.
    pub(crate) toolset: super::generation::Pinned,
    pub(crate) state: SessionState,
    pub(crate) messages: Vec<Message>,
    pub(crate) usage: Usage,
    pub(crate) steps: u64,
    pub(crate) ledger: Ledger,
    /// Tokens the caller has granted and not yet spent.
    pub(crate) credit_tokens: u64,
    pub(crate) budget: Budget,
    pub(crate) started_unix_millis: u64,
    /// Wall-clock time already charged to the budget, across previous requests.
    pub(crate) spent_wall_millis: u64,
    /// How many checkpoints this session has written. Bumped only by a successful one.
    pub(crate) version: u64,
    /// What this turn has debited so far. Reset at every turn, taken by a cancel.
    ///
    /// A cancelled attempt is dropped mid-stream, so the route never returns an `Ending` to record. The
    /// debits it made before the drop are real — the provider bills them — and without this the ledger
    /// would record the attempt as having cost nothing.
    pub(crate) turn_usage: Usage,
    /// The counter and the lock for this session's log.
    ///
    /// Not the handle's own: the kernel keeps one per `(tenant, session)` and hands the same `Arc` to
    /// every writer, because `load` builds a new handle per request and a copied counter is a counter
    /// that disagrees with itself. See `kernel/log.rs`.
    pub(crate) log: Arc<super::log::LogHead>,
    /// Calls waiting on a human. A batch can have several.
    pub(crate) pending: Vec<PendingCall>,
    /// The application's own state, opaque to the kernel.
    pub(crate) data: std::collections::BTreeMap<String, serde_json::Value>,
    /// Where commands run, and the default writable root.
    pub(crate) workspace: Option<String>,
    /// What commands may touch.
    pub(crate) sandbox: crate::contract::SandboxPolicy,
    /// The adapter hints in force.
    pub(crate) hints: crate::contract::Hints,
    /// How visible each tool is, and what a prompt and the pool may hold.
    pub(crate) tool_policy: crate::contract::ToolPolicy,
    /// Tools discovered or used, and the turn each was last reached — the pool that outlives a
    /// single prompt.
    pub(crate) discovered: std::collections::BTreeMap<String, u64>,
    /// Turns taken. The clock the pool expires against.
    pub(crate) round: u64,
    /// Why it stopped, once it has. The kernel decides this for a budget, a driver for anything
    /// else, and either way it belongs to the session rather than to a log the caller has to parse.
    pub(crate) stop: Option<crate::contract::StopReason>,
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

/// What it takes to open a session.
///
/// A struct rather than eight positional arguments: `Session::new` grew one argument at a time, and the
/// ones that look alike — a generation and its tools, a budget and its credit — are exactly the ones a
/// caller can transpose without the compiler noticing.
pub(crate) struct New {
    pub(crate) id: SessionId,
    pub(crate) principal: Principal,
    pub(crate) generation: Generation,
    pub(crate) tools: Arc<super::generation::ToolSet>,
    pub(crate) log: Arc<super::log::LogHead>,
    pub(crate) budget: Budget,
    pub(crate) credit_tokens: u64,
    pub(crate) now_unix_millis: u64,
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

    /// The standing instructions in force.
    #[must_use]
    pub fn system(&self) -> Option<&str> {
        self.system.as_deref()
    }

    /// How answers are shaped.
    #[must_use]
    pub fn params(&self) -> &crate::contract::GenerateParams {
        &self.params
    }

    /// The tool allow-list in force. Empty means every registered tool.
    #[must_use]
    pub fn tools(&self) -> &[String] {
        &self.tools
    }

    /// Who owns it. The tenant scopes the store key; the subject is the attribution.
    #[must_use]
    pub fn principal(&self) -> &crate::contract::Principal {
        &self.principal
    }

    /// Which tenant's session this is, for the store.
    #[must_use]
    pub fn key(&self) -> crate::adapter::SessionKey {
        crate::adapter::SessionKey {
            tenant: self.principal.tenant.clone(),
            id: self.id.clone(),
        }
    }

    /// The tool generation this session is pinned to.
    #[must_use]
    pub fn generation(&self) -> crate::contract::Generation {
        self.generation
    }

    /// Why the session stopped, once it has.
    #[must_use]
    pub fn stop_reason(&self) -> Option<crate::contract::StopReason> {
        self.stop
    }

    /// The exposure policy in force.
    #[must_use]
    pub fn tool_policy(&self) -> &crate::contract::ToolPolicy {
        &self.tool_policy
    }

    /// Tools this session has discovered or used, and the turn each was last reached.
    #[must_use]
    pub fn discovered(&self) -> &std::collections::BTreeMap<String, u64> {
        &self.discovered
    }

    /// The adapter hints in force.
    #[must_use]
    pub fn hints(&self) -> &crate::contract::Hints {
        &self.hints
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

    /// The counter and lock every writer to this session's log shares.
    #[must_use]
    pub(crate) fn log_head(&self) -> Arc<super::log::LogHead> {
        Arc::clone(&self.log)
    }

    /// The next event sequence number.
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.log.next()
    }

    pub(crate) fn new(init: New) -> Self {
        Self {
            id: init.id,
            principal: init.principal,
            generation: init.generation,
            toolset: super::generation::Pinned::new(init.tools),
            state: SessionState::Open,
            messages: Vec::new(),
            usage: Usage::default(),
            steps: 0,
            ledger: Ledger::default(),
            credit_tokens: init.credit_tokens,
            budget: init.budget,
            started_unix_millis: init.now_unix_millis,
            spent_wall_millis: 0,
            version: 0,
            turn_usage: Usage::default(),
            log: init.log,
            pending: Vec::new(),
            data: std::collections::BTreeMap::new(),
            workspace: None,
            sandbox: crate::contract::SandboxPolicy::default(),
            hints: crate::contract::Hints::default(),
            tool_policy: crate::contract::ToolPolicy::default(),
            discovered: std::collections::BTreeMap::new(),
            round: 0,
            stop: None,
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

    /// Close, and remember why.
    pub(crate) fn close_with(&mut self, reason: crate::contract::StopReason) -> Result<bool> {
        self.stop = Some(reason);
        self.close()
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
            && self.spent_wall_millis + now_unix_millis.saturating_sub(self.started_unix_millis)
                >= max
        {
            return Some(StopReason::Wall);
        }
        None
    }

    /// The checkpoint for the boundary the session is standing on.
    ///
    /// `None` before anything has been recorded: a snapshot that includes no event would claim
    /// a `seq` the log does not have.
    pub(crate) fn snapshot(&self, now_unix_millis: u64, version: u64) -> Option<Snapshot> {
        if self.log.next() == 0 {
            return None;
        }
        Some(Snapshot {
            session: self.id.clone(),
            principal: self.principal.clone(),
            seq: self.log.next() - 1,
            version,
            generation: self.generation,
            state: self.state,
            messages: self.messages.clone(),
            usage: self.usage.clone(),
            ledger: self.ledger.clone(),
            inference_calls: self.steps,
            credit_tokens: self.credit_tokens,
            budget: self.budget.clone(),
            // Charged to the clock now, because the moment this is written is the moment the session
            // stops running: the time after it belongs to the next request, not to this one.
            spent_wall_millis: self.spent_wall_millis
                + now_unix_millis.saturating_sub(self.started_unix_millis),
            pending: self.pending.clone(),
            routing: self.routing.clone(),
            system: self.system.clone(),
            tools: self.tools.clone(),
            params: self.params.clone(),
            workspace: self.workspace.clone(),
            sandbox: self.sandbox.clone(),
            hints: self.hints.clone(),
            tool_policy: self.tool_policy.clone(),
            discovered: self.discovered.clone(),
            round: self.round,
            stop: self.stop,
            kind: self.kind,
            process: self.process.clone(),
            data: self.data.clone(),
        })
    }

    /// Rebuild a session from a checkpoint and the spec the caller re-sent.
    pub(crate) fn from_snapshot(
        snapshot: Snapshot,
        principal: Principal,
        tools: Arc<super::generation::ToolSet>,
        log: Arc<super::log::LogHead>,
        budget: Budget,
        now_unix_millis: u64,
    ) -> Self {
        Self {
            id: snapshot.session,
            principal,
            generation: snapshot.generation,
            toolset: super::generation::Pinned::new(tools),
            state: snapshot.state,
            messages: snapshot.messages,
            usage: snapshot.usage,
            steps: snapshot.inference_calls,
            credit_tokens: snapshot.credit_tokens,
            ledger: snapshot.ledger,
            budget,
            // The clock restarts here and what it had already spent is restored beside it. The
            // alternative — one start time and a subtraction — would charge every idle gap between
            // requests to a session whose budget is about how long it runs, and a session parked on a
            // human verdict would exhaust its wall budget while nobody was looking at it.
            started_unix_millis: now_unix_millis,
            spent_wall_millis: snapshot.spent_wall_millis,
            version: snapshot.version,
            turn_usage: Usage::default(),
            // The counter belongs to the log, not to this handle: `load` may be handing out a second
            // handle for a session a supervisor is still writing to, and the head it resolves to is
            // already at the right number.
            log,
            pending: snapshot.pending,
            data: snapshot.data,
            // Restored, not zeroed: a resumed session that stopped retrying would be a session whose
            // behaviour changed because it was interrupted. Read before `routing` is moved.
            retries: snapshot.routing.retries,
            routing: snapshot.routing,
            params: snapshot.params,
            workspace: snapshot.workspace,
            sandbox: snapshot.sandbox,
            hints: snapshot.hints,
            tool_policy: snapshot.tool_policy,
            discovered: snapshot.discovered,
            round: snapshot.round,
            stop: snapshot.stop,
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
        Session::new(New {
            id: SessionId::new("s1"),
            principal: Principal {
                tenant: "acme".to_owned(),
                subject: "alice".to_owned(),
            },
            generation: Generation(1),
            tools: std::sync::Arc::new(crate::kernel::generation::ToolSet::default()),
            log: crate::kernel::log::LogHead::new(0),
            budget: Budget::default(),
            credit_tokens: 100,
            now_unix_millis: 0,
        })
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
        assert!(session().snapshot(0, 1).is_none());
    }
}
