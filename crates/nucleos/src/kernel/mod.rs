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
pub(crate) mod cancel;
pub(crate) mod config;
pub(crate) mod generation;
pub(crate) mod inference;
pub(crate) mod lifecycle;
pub(crate) mod process;
pub(crate) mod replay;
pub(crate) mod route;
pub(crate) mod sandbox;
pub(crate) mod session;
pub(crate) mod state;
pub(crate) mod tool_search;
pub(crate) mod tools;
pub(crate) mod writer;

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
use std::sync::{Arc, RwLock};

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
    /// The models a route may name. Behind a lock because configuration is changeable at runtime:
    /// adding one is a request, not a restart.
    models: Arc<RwLock<ModelRegistry>>,
    /// Every tool set a live session may be pinned to, plus the current one.
    generations: std::sync::RwLock<generation::Generations>,
    /// Turns that are running right now, so a cancel can reach the one it names.
    ///
    /// Keyed by session because a session runs one turn at a time: two concurrent inferences on one
    /// session would interleave in its message list, which the kernel has never allowed.
    turns: RwLock<std::collections::BTreeMap<SessionId, Arc<cancel::Cancel>>>,
    /// Per-tenant ceilings: what a tenant may use, whatever its sessions ask for.
    ///
    /// A second filter beside the session's own, not instead of it — the session's ceiling is the
    /// application's decision about one session, this is the deployment's about a tenant. Present with
    /// an empty list means "nothing", absent means "no tenant limit".
    tenant_tools:
        std::sync::RwLock<std::collections::BTreeMap<String, std::collections::BTreeSet<String>>>,
    /// Per-tenant model allow-lists. A route is filtered against these, so a fallback chain survives.
    tenant_models:
        std::sync::RwLock<std::collections::BTreeMap<String, std::collections::BTreeSet<String>>>,
    store: Arc<dyn Store>,
    /// Swappable: a deployment that can only tighten its policy by restarting will not tighten it.
    policy: Arc<RwLock<Arc<dyn Policy>>>,
    clock: Arc<dyn Clock>,
    heartbeat: std::time::Duration,
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
                    .map(|generations| {
                        generations
                            .current_set()
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                    })
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
    pub async fn events(
        &self,
        principal: &crate::contract::Principal,
        id: &SessionId,
        from_seq: u64,
    ) -> Result<Vec<Event>> {
        let key = crate::adapter::SessionKey {
            tenant: principal.tenant.clone(),
            id: id.clone(),
        };
        self.store.events_since(&key, from_seq).await
    }

    /// The registered model names, sorted.
    #[must_use]
    pub fn models(&self) -> Vec<String> {
        self.models
            .read()
            .map(|models| models.names())
            .unwrap_or_default()
    }
    /// The tool set new sessions are pinned to.
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.generations
            .read()
            .map(|generations| generations.current())
            .unwrap_or_default()
    }

    /// What the assistant had produced in a turn the runtime did not survive, if it stopped mid-turn.
    ///
    /// The fragment is in the event stream, not in `messages`; this reads it back so a caller can
    /// decide what to do — report it, or continue the conversation from it. It cannot be resumed:
    /// no provider continues a generation from the middle.
    pub async fn partial(
        &self,
        session: &session::Session,
    ) -> Result<Option<crate::contract::PartialTurn>> {
        let events = self.store.events_since(&session.key(), 0).await?;
        Ok(replay::partial(&events))
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

    /// Forget every tool set but the current one.
    ///
    /// Sessions keep working: each holds the set its calls are answered from, so this frees what nobody
    /// holds and nothing else. It is a request rather than something `publish` does on its own, because
    /// an operator reloading sources should not have to wonder which generations survived — they ask,
    /// and the answer says how many were forgotten.
    ///
    /// Never retires the current set: a session migrating at its next turn boundary has to have
    /// somewhere to migrate to.
    pub fn retire(&self) -> Result<Vec<Generation>> {
        let mut generations = self
            .generations
            .write()
            .map_err(|_| Error::internal("the tool registry is poisoned"))?;
        let retired = generations.retire();
        let kept = generations.len();
        drop(generations);
        tracing::info!(event = "generations_retired", retired = retired.len(), kept,);
        Ok(retired)
    }

    /// The sets this kernel is holding, for a reader that wants to know.
    pub(crate) fn generations(&self) -> std::sync::RwLockReadGuard<'_, generation::Generations> {
        self.generations
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Resolve a route against the registry.
    pub fn resolve_route(&self, routing: &crate::contract::Routing) -> Result<ResolvedRoute> {
        // Resolved out of the lock rather than inside it: the returned `Arc`s are what the call needs,
        // and holding a read guard across an await would make every caller's future non-`Send`.
        self.models
            .read()
            .map_err(|_| Error::internal("the model registry lock is poisoned"))?
            .resolve(routing)
    }
}

/// Assembles a [`Kernel`].
pub struct Builder {
    models: ModelRegistry,
    /// The first generation's tools. Publishing more is [`Kernel::publish`], after it is built.
    tools: Vec<Arc<dyn Tool>>,
    store: Option<Arc<dyn Store>>,
    policy: Option<Arc<dyn Policy>>,
    clock: Option<Arc<dyn Clock>>,
    heartbeat: std::time::Duration,
    retries: u32,
    routing: Routing,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            models: ModelRegistry::default(),
            tools: Vec::new(),
            store: None,
            policy: None,
            clock: None,
            // Fifteen seconds: long enough that a fast tool is never interrupted by a record about
            // itself, short enough that a caller waiting on a slow one learns it is alive.
            heartbeat: std::time::Duration::from_secs(15),
            retries: 0,
            routing: Routing::default(),
        }
    }
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

    /// How long a call may be silent before the kernel says it is still running.
    ///
    /// A policy rather than a constant: a deployment with fast tools wants a longer window than one
    /// with slow builds, and a test wants milliseconds. Zero disables the heartbeat.
    #[must_use]
    pub fn heartbeat(mut self, every: std::time::Duration) -> Self {
        self.heartbeat = every;
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
            models: Arc::new(RwLock::new(self.models)),
            generations: std::sync::RwLock::new(generation::Generations::first(self.tools)?),
            turns: RwLock::new(std::collections::BTreeMap::new()),
            tenant_tools: std::sync::RwLock::new(std::collections::BTreeMap::new()),
            tenant_models: std::sync::RwLock::new(std::collections::BTreeMap::new()),
            store: self
                .store
                .ok_or_else(|| Error::internal("kernel needs a store"))?,
            policy: Arc::new(RwLock::new(
                self.policy
                    .ok_or_else(|| Error::internal("kernel needs a policy"))?,
            )),
            clock: self.clock.unwrap_or_else(|| Arc::new(SystemClock)),
            heartbeat: self.heartbeat,
            retries: self.retries,
            routing: self.routing,
            supervisor: process::Supervisor::default(),
        })
    }
}

impl Kernel {
    pub(crate) fn policy(&self) -> Arc<dyn Policy> {
        self.policy
            .read()
            .map(|policy| Arc::clone(&policy))
            .unwrap_or_else(|poisoned| Arc::clone(&poisoned.into_inner()))
    }

    /// Replace the policy in force.
    ///
    /// The next call sees it; a call already in flight keeps the one it started with, which is the
    /// same rule session configuration follows. Recorded by the caller, because a policy change with
    /// no line in a record is indistinguishable afterwards from a bug.
    pub fn set_policy(&self, policy: Arc<dyn Policy>) -> Result<()> {
        let mut current = self
            .policy
            .write()
            .map_err(|_| Error::internal("the policy lock is poisoned"))?;
        *current = policy;
        Ok(())
    }

    /// Cancel the turn a session is running. `true` if one was.
    ///
    /// The turn returns `Cancelled`, the session stays open, and what the attempt had produced stays in
    /// the log — cancelling stops a call, it does not undo a conversation.
    pub fn cancel(&self, session: &SessionId) -> bool {
        let turns = self
            .turns
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match turns.get(session) {
            Some(cancel) => {
                cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Register a turn so a cancel can find it.
    pub(crate) fn begin_turn(&self, session: &SessionId) -> Arc<cancel::Cancel> {
        let cancel = Arc::new(cancel::Cancel::new());
        self.turns
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session.clone(), Arc::clone(&cancel));
        cancel
    }

    /// Forget a turn once it has ended, however it ended.
    pub(crate) fn end_turn(&self, session: &SessionId) {
        self.turns
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session);
    }

    /// Restrict what one tenant may use. `None` clears the limit; `Some(vec![])` allows nothing.
    ///
    /// Enforced where the session's own ceiling is enforced, so a tightening reaches sessions that are
    /// already running — which is the point of a limit an operator can set during an incident.
    pub fn set_tenant_tools(&self, tenant: &str, tools: Option<Vec<String>>) -> Result<()> {
        set_limit(&self.tenant_tools, tenant, tools)
    }

    /// Restrict which models one tenant may route to.
    pub fn set_tenant_models(&self, tenant: &str, models: Option<Vec<String>>) -> Result<()> {
        set_limit(&self.tenant_models, tenant, models)
    }

    /// Whether a tenant may use a tool. Absent limit means yes.
    pub(crate) fn tenant_allows_tool(&self, tenant: &str, name: &str) -> bool {
        match self
            .tenant_tools
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(tenant)
        {
            Some(allowed) => allowed.contains(name),
            None => true,
        }
    }

    /// Filter a resolved route against the tenant's allow-list.
    ///
    /// Filtered rather than refused: a chain that names a forbidden model and an allowed fallback keeps
    /// working, which is what a deployment with per-tenant access expects. Only when nothing survives is
    /// the call refused, and then it says which tenant and why.
    pub(crate) fn permit_route(
        &self,
        principal: &crate::contract::Principal,
        targets: ResolvedRoute,
    ) -> Result<ResolvedRoute> {
        let limits = self
            .tenant_models
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(allowed) = limits.get(&principal.tenant) else {
            return Ok(targets);
        };
        let kept: ResolvedRoute = targets
            .into_iter()
            .filter(|(name, _)| allowed.contains(name))
            .collect();
        if kept.is_empty() {
            return Err(Error::denied(format!(
                "no model in this route is available to tenant {:?}",
                principal.tenant
            )));
        }
        Ok(kept)
    }

    /// Register a model for routes to name.
    ///
    /// Replacing an existing name is allowed and takes effect on the next route resolution; sessions
    /// already in a call finish on the model they started with.
    pub fn add_model(&self, name: &str, model: Arc<dyn Model>) -> Result<()> {
        let mut models = self
            .models
            .write()
            .map_err(|_| Error::internal("the model registry lock is poisoned"))?;
        models.insert(name, model);
        Ok(())
    }

    /// Forget a model. Routes that still name it fail at resolution, by name.
    pub fn remove_model(&self, name: &str) -> Result<bool> {
        let mut models = self
            .models
            .write()
            .map_err(|_| Error::internal("the model registry lock is poisoned"))?;
        Ok(models.remove(name))
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

/// Store or clear one tenant limit.
fn set_limit(
    limits: &std::sync::RwLock<
        std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    >,
    tenant: &str,
    names: Option<Vec<String>>,
) -> Result<()> {
    let mut limits = limits
        .write()
        .map_err(|_| Error::internal("the tenant limit lock is poisoned"))?;
    match names {
        Some(names) => {
            limits.insert(tenant.to_owned(), names.into_iter().collect());
        }
        None => {
            limits.remove(tenant);
        }
    }
    Ok(())
}
