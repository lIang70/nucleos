//! The kernel: what a host assembles, and what it calls.
//!
//! Everything here is wiring. The rules live in the session state machine (`session.rs`), the
//! route walk (`route.rs`) and the turn loop (`turn.rs`).

pub(crate) mod route;
pub(crate) mod session;
pub(crate) mod turn;

use crate::adapter::{
    Clock, GenerateParams, Model, ModelRegistry, Policy, Store, SystemClock, Tool,
};
use crate::contract::{
    Budget, Event, EventKind, Generation, Ledger, Message, Principal, Routing, SessionId,
    SessionState, StopReason,
};
use crate::error::{Error, Result};
use std::collections::BTreeMap;
use std::sync::Arc;

/// What a caller asks for when starting or resuming a session.
#[derive(Debug, Clone, Default)]
pub struct SessionSpec {
    /// The route. `None` uses the kernel's default.
    pub routing: Option<Routing>,
    /// Standing instructions.
    pub system: Option<String>,
    /// The opening messages.
    pub initial: Vec<Message>,
    /// Tool names this session may use. Empty means every registered tool.
    pub tools: Vec<String>,
    /// How to shape answers.
    pub params: GenerateParams,
    /// What the session may consume.
    pub budget: Budget,
    /// Tokens granted up front, spent by usage. Zero means "no credit, no model calls".
    pub credit_tokens: u64,
}

/// How a session ended, or where it stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Which session.
    pub session: SessionId,
    /// Where it is.
    pub state: SessionState,
    /// Why it stopped, when it did.
    pub stop: Option<StopReason>,
    /// What it spent, attempt by attempt.
    pub ledger: Ledger,
    /// The next event sequence number — a client's cursor after replay.
    pub next_seq: u64,
}

impl Outcome {
    /// Whether the session reached a terminal state.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }
}

/// A human's answer to a parked call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitlVerdict {
    /// Which call was gated.
    pub tool_call_id: String,
    /// Which option was offered and picked.
    pub option_id: String,
}

/// A route resolved against the registry: names paired with the models behind them.
pub(crate) type ResolvedRoute = Vec<(String, Arc<dyn Model>)>;

/// The kernel.
pub struct Kernel {
    models: ModelRegistry,
    tools: BTreeMap<String, Arc<dyn Tool>>,
    store: Arc<dyn Store>,
    policy: Arc<dyn Policy>,
    clock: Arc<dyn Clock>,
    generation: Generation,
    retries: u32,
    routing: Routing,
}

impl std::fmt::Debug for Kernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Adapters are not `Debug`; the names are what a reader wants.
        f.debug_struct("Kernel")
            .field("models", &self.models)
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .field("generation", &self.generation)
            .field("routing", &self.routing)
            .finish_non_exhaustive()
    }
}

impl Kernel {
    /// Start assembling a kernel.
    #[must_use]
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// Create a session and drive it until it stops.
    pub async fn start(
        &self,
        principal: &Principal,
        id: SessionId,
        spec: SessionSpec,
    ) -> Result<Outcome> {
        let (routing, targets) = self.resolve(&spec)?;
        let now = self.clock.now_unix_millis();
        let mut session = session::Session::new(
            id,
            principal.clone(),
            self.generation,
            spec.budget.clone(),
            spec.credit_tokens,
            now,
        );
        session.messages = self.opening_messages(&spec);
        self.drive(session, spec, routing, targets).await
    }

    /// The events a session has recorded, from `from_seq` onward.
    ///
    /// The daemon's `Run` is a view of this: it replays, then ends. Resumption is the property
    /// that makes a dropped connection harmless, and it needs nothing but the log.
    pub async fn events(&self, id: &SessionId, from_seq: u64) -> Result<Vec<Event>> {
        self.store.events_since(id, from_seq).await
    }

    /// The registered model names, sorted.
    #[must_use]
    pub fn models(&self) -> Vec<String> {
        self.models.names()
    }

    /// Pick up a parked or interrupted session and keep going.
    ///
    /// The spec is re-sent rather than remembered: a route is a deployment decision that may
    /// have changed while the session waited, and the log does not need to carry it.
    pub async fn resume(
        &self,
        principal: &Principal,
        id: SessionId,
        spec: SessionSpec,
    ) -> Result<Outcome> {
        let snapshot = self
            .store
            .snapshot(&id)
            .await?
            .ok_or_else(|| Error::not_found(format!("no checkpoint for session {id}")))?;
        let (routing, targets) = self.resolve(&spec)?;
        let now = self.clock.now_unix_millis();
        let session = session::Session::from_snapshot(
            snapshot,
            principal.clone(),
            spec.budget.clone(),
            spec.credit_tokens,
            now,
        );
        self.drive(session, spec, routing, targets).await
    }

    /// Answer the call a session is parked on, then keep going.
    pub async fn answer(
        &self,
        principal: &Principal,
        id: SessionId,
        spec: SessionSpec,
        verdict: HitlVerdict,
    ) -> Result<Outcome> {
        let snapshot = self
            .store
            .snapshot(&id)
            .await?
            .ok_or_else(|| Error::not_found(format!("no checkpoint for session {id}")))?;
        let pending = snapshot
            .pending
            .clone()
            .ok_or_else(|| Error::invalid(format!("session {id} is not waiting on a human")))?;
        if pending.call.id != verdict.tool_call_id {
            return Err(Error::invalid(format!(
                "verdict names {} but the session waits on {}",
                verdict.tool_call_id, pending.call.id
            )));
        }
        let option = pending
            .options
            .iter()
            .find(|option| option.id == verdict.option_id)
            .ok_or_else(|| {
                Error::invalid(format!("option {:?} was not offered", verdict.option_id))
            })?;

        let (routing, targets) = self.resolve(&spec)?;
        let now = self.clock.now_unix_millis();
        let mut session = session::Session::from_snapshot(
            snapshot,
            principal.clone(),
            spec.budget.clone(),
            spec.credit_tokens,
            now,
        );
        session.pending = None;
        session.transition(SessionState::Running)?;
        self.emit_state(&mut session).await?;

        // The verdict becomes the call's result: allowed ones run, refused ones answer with
        // content the model can read.
        let allow = matches!(
            option.kind,
            crate::contract::PermissionKind::AllowOnce
                | crate::contract::PermissionKind::AllowAlways
        );
        let output = if allow {
            self.invoke_tool(&pending.call, &session.id, u64::MAX).await
        } else {
            crate::adapter::ToolOutput::failed("refused by a human")
        };
        self.finish_call(&mut session, &pending.call, output, 0)
            .await?;
        self.checkpoint(&session).await?;
        self.drive(session, spec, routing, targets).await
    }

    // -- wiring ---------------------------------------------------------------

    fn resolve(&self, spec: &SessionSpec) -> Result<(Routing, ResolvedRoute)> {
        let routing = spec.routing.clone().unwrap_or_else(|| self.routing.clone());
        let mut with_retries = routing.clone();
        with_retries.retries = routing.retries.max(self.retries);
        let targets = self.models.resolve(&with_retries)?;
        Ok((with_retries, targets))
    }

    fn opening_messages(&self, spec: &SessionSpec) -> Vec<Message> {
        spec.initial.clone()
    }

    /// One tool by name.
    pub(crate) fn tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// The declarations this session may use, in name order so a prompt is reproducible.
    pub(crate) fn tool_specs(&self, session: &session::Session) -> Vec<crate::contract::ToolSpec> {
        self.tools
            .iter()
            .filter(|(name, _)| session.tools.is_empty() || session.tools.contains(name))
            .map(|(_, tool)| tool.spec())
            .collect()
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

    pub(crate) async fn emit_state(&self, session: &mut session::Session) -> Result<()> {
        let kind = EventKind::StateChanged {
            state: session.state,
            stop: None,
        };
        self.emit(session, kind).await
    }

    /// Write the checkpoint for the boundary the session is standing on.
    pub(crate) async fn checkpoint(&self, session: &session::Session) -> Result<()> {
        match session.snapshot() {
            Some(snapshot) => self.store.write_snapshot(snapshot).await,
            None => Ok(()),
        }
    }

    fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }
}

/// Assembles a [`Kernel`].
#[derive(Default)]
pub struct Builder {
    models: ModelRegistry,
    tools: BTreeMap<String, Arc<dyn Tool>>,
    store: Option<Arc<dyn Store>>,
    policy: Option<Arc<dyn Policy>>,
    clock: Option<Arc<dyn Clock>>,
    generation: Generation,
    retries: u32,
    routing: Routing,
}

impl Builder {
    /// Register a model under a name a route can name.
    #[must_use]
    pub fn model(mut self, name: impl Into<String>, model: Arc<dyn Model>) -> Self {
        self.models.insert(name, model);
        self
    }

    /// Register a tool. Its own `spec().name` is the name a route and a session use.
    #[must_use]
    pub fn tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.insert(tool.spec().name, tool);
        self
    }

    /// Set the event log.
    #[must_use]
    pub fn store(mut self, store: Arc<dyn Store>) -> Self {
        self.store = Some(store);
        self
    }

    /// Set the policy.
    #[must_use]
    pub fn policy(mut self, policy: Arc<dyn Policy>) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Set the clock.
    #[must_use]
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// The tool-source generation new sessions are pinned to.
    #[must_use]
    pub fn generation(mut self, generation: Generation) -> Self {
        self.generation = generation;
        self
    }

    /// Extra attempts per target, on top of whatever a session asks for.
    #[must_use]
    pub fn retries(mut self, retries: u32) -> Self {
        self.retries = retries;
        self
    }

    /// The route a session uses when it does not name one.
    #[must_use]
    pub fn routing(mut self, routing: Routing) -> Self {
        self.routing = routing;
        self
    }

    /// Finish.
    ///
    /// A kernel without a store, a policy or a clock has no defaults to fall back on: a
    /// silently invented policy is how "deny by default" turns into "allow by accident".
    pub fn build(self) -> Result<Kernel> {
        Ok(Kernel {
            models: self.models,
            tools: self.tools,
            store: self
                .store
                .ok_or_else(|| Error::internal("kernel needs a store"))?,
            policy: self
                .policy
                .ok_or_else(|| Error::internal("kernel needs a policy"))?,
            clock: self.clock.unwrap_or_else(|| Arc::new(SystemClock)),
            generation: self.generation,
            retries: self.retries,
            routing: self.routing,
        })
    }
}

impl Kernel {
    pub(crate) fn policy(&self) -> &Arc<dyn Policy> {
        &self.policy
    }

    pub(crate) fn now(&self) -> u64 {
        self.clock().now_unix_millis()
    }
}
