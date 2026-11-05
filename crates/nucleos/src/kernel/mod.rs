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
pub(crate) mod live;
pub(crate) mod log;
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
    /// Tokens granted up front. `None` means no limit, which is the default.
    pub credit_tokens: Option<u64>,
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
    /// The events this kernel has written, for anything watching a turn as it runs.
    live: live::Hub,
    /// The head of every log this kernel is writing to.
    ///
    /// Keyed by the log, not by the handle: `load` mints a handle per request, and a counter that is
    /// copied per handle is a counter that disagrees with itself. See `kernel/log.rs`.
    logs: std::sync::Mutex<
        std::collections::BTreeMap<crate::adapter::SessionKey, std::sync::Arc<log::LogHead>>,
    >,
    /// Turns that are running right now, so a cancel can reach the one it names.
    ///
    /// Keyed by `(tenant, session)`, like every other per-session thing: session ids are chosen by
    /// callers, so two tenants each naming one `s1` is ordinary. Keyed by id alone, one tenant's
    /// `end_turn` could retire another tenant's turn and its `cancel` would address the wrong one.
    turns: RwLock<std::collections::BTreeMap<crate::adapter::SessionKey, Arc<cancel::Cancel>>>,
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
    /// The fragment an unfinished turn left behind, if any.
    ///
    /// Read **backwards** from the head, a page at a time, and stop at the first boundary. The
    /// fragment is the tail of the log; reading from seq 0 made `GET /v1/sessions/{id}` cost the
    /// session's whole history, on the endpoint clients poll.
    ///
    /// The pages are capped: past `PARTIAL_CAP` events the fragment is not a fragment, and reporting
    /// nothing is a better answer than reading a history to find one.
    pub async fn partial(
        &self,
        session: &session::Session,
    ) -> Result<Option<crate::contract::PartialTurn>> {
        const PAGE: usize = 256;
        const CAP: usize = 8 * PAGE;

        let key = session.key();
        let mut end = session.next_seq();
        let mut collected: Vec<crate::contract::Event> = Vec::new();
        while collected.len() < CAP && end > 0 {
            let page = self.store.events_before(&key, end, PAGE).await?;
            let Some(first) = page.first() else {
                break;
            };
            end = first.seq;
            let mut boundary = false;
            for event in page.iter().rev() {
                collected.push(event.clone());
                if replay::ends_the_fragment(&event.kind) {
                    boundary = true;
                    break;
                }
            }
            if boundary {
                break;
            }
        }
        collected.reverse();
        Ok(replay::partial(&collected))
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
            live: live::Hub::new(),
            logs: std::sync::Mutex::new(std::collections::BTreeMap::new()),
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

    /// The head of a session's log, created on first use.
    ///
    /// `next_seq` is where the log continues *if this kernel has not seen it before*: `open` passes 0,
    /// `load` passes the snapshot's last sequence plus one. An existing head wins, because a head only
    /// exists while something is writing — a process supervisor, a turn — and its counter is the one that
    /// is current.
    pub(crate) fn log_head(
        &self,
        key: &crate::adapter::SessionKey,
        next_seq: u64,
    ) -> std::sync::Arc<log::LogHead> {
        let mut logs = self
            .logs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::sync::Arc::clone(
            logs.entry(key.clone())
                .or_insert_with(|| log::LogHead::new(next_seq)),
        )
    }

    /// Forget the head of a session that can no longer be written to.
    ///
    /// Only the kernel's own entry: a supervisor or a turn that still holds the `Arc` keeps its
    /// counter, which is exactly what it should do — the entry in the map is for *future* writers,
    /// and a closed session has none.
    pub(crate) fn forget_log(&self, key: &crate::adapter::SessionKey) {
        self.logs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(key);
    }

    /// The head for a read, without registering it.
    ///
    /// `load_head` is the view path, and a view writes nothing: creating a head in the map for every
    /// session anybody ever looked at would keep one counter per session for the daemon's life, even
    /// for sessions that are never driven again. An existing head is shared; an absent one is
    /// constructed for this read and dropped with it.
    pub(crate) fn peek_log_head(
        &self,
        key: &crate::adapter::SessionKey,
        next_seq: u64,
    ) -> std::sync::Arc<log::LogHead> {
        if let Some(head) = self
            .logs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
        {
            return std::sync::Arc::clone(head);
        }
        log::LogHead::new(next_seq)
    }

    /// Remove a session: its log, its checkpoint, and the head this kernel was holding for it.
    ///
    /// Only a closed session may be removed — deleting an open one would destroy a conversation
    /// somebody may still be using, and the answer to "I want it gone" for a running session is
    /// `close` first. Returns whether anything was there.
    pub async fn delete(
        &self,
        session: &SessionId,
        principal: &crate::contract::Principal,
    ) -> Result<bool> {
        let key = crate::adapter::SessionKey {
            tenant: principal.tenant.clone(),
            id: session.clone(),
        };
        // Ownership is the store's key, and the state is the checkpoint's: both are answered before
        // anything is removed, because deletion is the one operation there is no undo for.
        let snapshot = self.store.snapshot(&key).await?;
        let Some(snapshot) = snapshot else {
            return Ok(false);
        };
        if snapshot.principal.subject != principal.subject {
            return Err(Error::not_found(format!("no session {session}")));
        }
        if snapshot.state == crate::contract::SessionState::Open {
            return Err(Error::new(
                crate::contract::ErrorCode::Conflict,
                format!("session {session} is open: close it before removing it"),
            ));
        }
        let removed = self.store.delete(&key).await?;
        self.logs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&key);
        tracing::info!(event = "session_deleted", session = %session, tenant = %principal.tenant);
        Ok(removed)
    }

    /// Remove closed sessions whose last activity is before a deadline, and forget their log heads.
    ///
    /// The store answers what is old and finished; the kernel forgets the in-memory head for each one,
    /// so a later `open` of the same id starts a new log rather than continuing a deleted one.
    pub async fn prune_closed(
        &self,
        query: &crate::adapter::PruneQuery,
    ) -> Result<crate::adapter::PruneReport> {
        let report = self.store.prune_closed(query).await?;
        if !query.dry_run {
            for key in &report.candidates {
                self.forget_log(key);
            }
        }
        Ok(report)
    }

    /// One bounded page of a session's events.
    ///
    /// What the SSE handler loops over: a page at a time, so the first frame does not wait for the
    /// last piece of history and memory does not grow with the session's age.
    pub async fn events_page(
        &self,
        principal: &crate::contract::Principal,
        id: &SessionId,
        from_seq: u64,
        limit: usize,
    ) -> Result<Vec<crate::contract::Event>> {
        let key = crate::adapter::SessionKey {
            tenant: principal.tenant.clone(),
            id: id.clone(),
        };
        self.store.events_page(&key, from_seq, limit).await
    }

    /// Follow the events this kernel writes, from now on.
    ///
    /// Subscribe **before** starting the turn: the kernel announces events as it writes them, so a
    /// follower that attaches afterwards has missed whatever was already written. What it missed is in
    /// the store — a lagging or late follower catches up with `events(from_seq)`, which is why this
    /// returns a plain receiver rather than a promise of completeness.
    #[must_use]
    pub fn follow(&self) -> tokio::sync::broadcast::Receiver<live::Live> {
        self.live.follow()
    }

    /// Cancel the turn a session is running. `true` if one was.
    ///
    /// The turn returns `Cancelled`, the session stays open, and what the attempt had produced stays in
    /// the log — cancelling stops a call, it does not undo a conversation.
    pub fn cancel(&self, session: &crate::adapter::SessionKey) -> bool {
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
    pub(crate) fn begin_turn(&self, session: &crate::adapter::SessionKey) -> Arc<cancel::Cancel> {
        let cancel = Arc::new(cancel::Cancel::new());
        self.turns
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session.clone(), Arc::clone(&cancel));
        cancel
    }

    /// Forget a turn once it has ended, however it ended.
    /// Retire the turn that registered *this* signal.
    ///
    /// Pointer equality, not key equality: a second turn on the same session replaces the entry, and a
    /// first turn that finished afterwards must not delete its successor's registration — otherwise
    /// `cancel` answers "nothing running" for a turn that is still running and billing.
    pub(crate) fn end_turn(
        &self,
        session: &crate::adapter::SessionKey,
        mine: &Arc<cancel::Cancel>,
    ) {
        let mut turns = self
            .turns
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(registered) = turns.get(session)
            && Arc::ptr_eq(registered, mine)
        {
            turns.remove(session);
        }
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
