//! The kernel's syscall surface.
//!
//! An agent loop is a driver: `open`, then some number of `inference` and `invoke_tools` calls,
//! then `close`. That loop is not here — it is in `nucleos-agent` — because a kernel that runs
//! your application's loop has taken a position on what your application should be.
//!
//! What stays is everything a driver cannot be trusted to reimplement: the route walk with its
//! retries and its ledger (`route.rs`), the session's state and its checkpoint (`session.rs`),
//! policy, and the tool set a generation pinned.

pub(crate) mod bm25;
pub(crate) mod budget;
pub(crate) mod generation;
pub(crate) mod config;
pub(crate) mod inference;
pub(crate) mod lifecycle;
pub(crate) mod process;
pub(crate) mod route;
pub(crate) mod sandbox;
pub(crate) mod session;
pub(crate) mod state;
pub(crate) mod tool_search;
pub(crate) mod tools;

pub use session::Session;

/// A write to the session's own state.
#[derive(Debug, Clone, Default)]
pub struct StateWrite {
    /// Messages to append to the conversation.
    pub messages: Vec<Message>,
    /// Keys to set in the application's opaque state.
    pub data: std::collections::BTreeMap<String, serde_json::Value>,
}

/// What one inference produced.
#[derive(Debug, Clone)]
pub struct InferenceOutcome {
    /// The assistant message. Appending it to the conversation is the caller's decision.
    pub message: Message,
    /// The provider's own reason for stopping — the least trustworthy of the stop conditions.
    pub finish: crate::adapter::FinishReason,
    /// What the session has spent so far.
    pub usage: Usage,
}

use crate::adapter::{Clock, Model, ModelRegistry, Policy, Store, SystemClock, Tool};
use crate::contract::{
    Event, Generation, Ledger, Message, Routing, SessionId, SessionState, StopReason, Usage,
};
use crate::error::{Error, Result};
use std::sync::Arc;

/// What a caller asks for when starting or resuming a session.
#[derive(Debug, Clone, Default)]
pub struct SessionSpec {
    /// What this session is for.
    pub kind: crate::contract::SessionKind,
    /// How to start it, when it is a process session.
    pub process: Option<crate::contract::ProcessSpec>,
    /// What its commands may touch. Not changeable while it runs.
    pub sandbox: crate::contract::SandboxPolicy,
    /// Where its commands run.
    pub workspace: Option<String>,
    /// Messages to seed the conversation with.
    pub initial: Vec<Message>,
    /// Tokens granted up front.
    pub credit_tokens: u64,
    /// What the session may consume. A limit, not content: raising it is `Credit`, not `configure`.
    pub budget: crate::contract::Budget,
    /// The mutable configuration to start from.
    ///
    /// A patch rather than a set of fields: it is the same shape [`Kernel::configure`] takes, so a
    /// session can start in any state it could later be changed into — and there is one place that
    /// validates a configuration, not two.
    pub config: crate::contract::ConfigPatch,
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
    /// Whether the session accepts no further calls.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state.is_closed()
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
    /// Every tool set a live session may be pinned to, plus the current one.
    generations: std::sync::RwLock<generation::Generations>,
    store: Arc<dyn Store>,
    policy: Arc<dyn Policy>,
    clock: Arc<dyn Clock>,
    retries: u32,
    routing: Routing,
    /// The processes this kernel is supervising.
    ///
    /// In the kernel because supervision is mechanism, not transport: the same `Arc<Kernel>` is
    /// held for the daemon's life, and the tool registry already lives here.
    supervisor: process::Supervisor,
}

impl std::fmt::Debug for Kernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Adapters are not `Debug`; the names are what a reader wants.
        f.debug_struct("Kernel")
            .field("models", &self.models)
            .field(
                "tools",
                &self
                    .generations
                    .read()
                    .map(|generations| generations.current_set().keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default(),
            )
            .field("generation", &self.generation())
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

    /// The events a session has recorded, from `from_seq` onward.
    pub async fn events(&self, id: &SessionId, from_seq: u64) -> Result<Vec<Event>> {
        self.store.events_since(id, from_seq).await
    }

    /// The registered model names, sorted.
    #[must_use]
    pub fn models(&self) -> Vec<String> {
        self.models.names()
    }
    /// The tool set new sessions are pinned to.
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.generations
            .read()
            .map(|generations| generations.current())
            .unwrap_or_default()
    }

    /// Publish a new tool set, and pin new sessions to it.
    ///
    /// Existing sessions keep theirs until they reach a turn boundary and the sets are compatible.
    /// A session that never migrates keeps working; that is the point of pinning.
    pub fn publish(&self, tools: Vec<Arc<dyn Tool>>) -> Result<Generation> {
        let mut generations = self
            .generations
            .write()
            .map_err(|_| Error::internal("the tool registry is poisoned"))?;
        generations.publish(tools)
    }

    /// The sets this kernel is holding, for a reader that wants to know.
    pub(crate) fn generations(&self) -> std::sync::RwLockReadGuard<'_, generation::Generations> {
        self.generations
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Resolve a route against the registry.
    pub fn resolve_route(&self, routing: &crate::contract::Routing) -> Result<ResolvedRoute> {
        self.models.resolve(routing)
    }
}

/// Assembles a [`Kernel`].
#[derive(Default)]
pub struct Builder {
    models: ModelRegistry,
    /// The first generation's tools. Publishing more is [`Kernel::publish`], after it is built.
    tools: Vec<Arc<dyn Tool>>,
    store: Option<Arc<dyn Store>>,
    policy: Option<Arc<dyn Policy>>,
    clock: Option<Arc<dyn Clock>>,
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
        self.tools.push(tool);
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
            generations: std::sync::RwLock::new(generation::Generations::first(self.tools)?),
            store: self
                .store
                .ok_or_else(|| Error::internal("kernel needs a store"))?,
            policy: self
                .policy
                .ok_or_else(|| Error::internal("kernel needs a policy"))?,
            clock: self.clock.unwrap_or_else(|| Arc::new(SystemClock)),
            retries: self.retries,
            routing: self.routing,
            supervisor: process::Supervisor::default(),
        })
    }
}

impl Kernel {
    pub(crate) fn policy(&self) -> &Arc<dyn Policy> {
        &self.policy
    }

    /// The current time, from the injected clock.
    #[must_use]
    pub fn now(&self) -> u64 {
        self.clock().now_unix_millis()
    }

    /// The wall-clock deadline a call inherits from the session.
    #[must_use]
    pub fn deadline(&self, session: &session::Session) -> u64 {
        match session.budget.max_wall_millis {
            Some(max) => session.started_unix_millis.saturating_add(max),
            None => u64::MAX,
        }
    }
}
