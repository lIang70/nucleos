//! The kernel's syscall surface.
//!
//! An agent loop is a driver: `open`, then some number of `inference` and `invoke_tools` calls,
//! then `close`. That loop is not here — it is in `nucleos-agent` — because a kernel that runs
//! your application's loop has taken a position on what your application should be.
//!
//! What stays is everything a driver cannot be trusted to reimplement: the route walk with its
//! retries and its ledger (`route.rs`), the session's state and its checkpoint (`session.rs`),
//! policy, and the tool set a generation pinned.

pub(crate) mod route;
pub(crate) mod session;

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

use crate::adapter::{
    Clock, GenerateParams, Model, ModelRegistry, Policy, Store, SystemClock, Tool, ToolOutput,
};
use crate::contract::{
    Budget, Content, Event, EventKind, Generation, Ledger, Message, Part, PartKind, Principal,
    Routing, SessionId, SessionState, StopReason, ToolResultPart, Usage,
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

    // -- session lifecycle ----------------------------------------------------

    /// Open a session.
    pub async fn open(
        &self,
        principal: &Principal,
        id: SessionId,
        spec: SessionSpec,
    ) -> Result<session::Session> {
        let now = self.now();
        let mut session = session::Session::new(
            id,
            principal.clone(),
            self.generation,
            spec.budget.clone(),
            spec.credit_tokens,
            now,
        );
        session.routing = spec.routing.clone().unwrap_or_else(|| self.routing.clone());
        session.retries = session.routing.retries.max(self.retries);
        session.params = spec.params.clone();
        session.system = spec.system.clone();
        session.tools = spec.tools.clone();
        session.messages = spec.initial.clone();
        // Fail before anything is recorded: a route naming a model that does not exist is a
        // configuration error, not a session that fails on its first turn.
        self.models.resolve(&session.routing)?;
        self.emit_state(&mut session).await?;
        self.write_checkpoint(&session).await?;
        Ok(session)
    }

    /// Load a session from its checkpoint.
    ///
    /// The spec is not re-sent: the checkpoint carries the route, the parameters and the tool set,
    /// because a resume that had to be told its own configuration again is a resume that can
    /// silently behave differently.
    pub async fn load(&self, principal: &Principal, id: &SessionId) -> Result<session::Session> {
        let snapshot = self
            .store
            .snapshot(id)
            .await?
            .ok_or_else(|| Error::not_found(format!("no checkpoint for session {id}")))?;
        Ok(session::Session::from_snapshot(
            snapshot,
            principal.clone(),
            Default::default(),
            self.now(),
        ))
    }

    /// Close a session. Further calls are refused.
    pub async fn close(&self, session: &mut session::Session, reason: StopReason) -> Result<()> {
        if !session.close()? {
            return Ok(());
        }
        self.emit(
            session,
            EventKind::StateChanged {
                state: SessionState::Closed,
                stop: Some(reason),
            },
        )
        .await?;
        self.write_checkpoint(session).await
    }

    // -- syscalls -------------------------------------------------------------

    /// One inference: walk the route, emit the deltas, record every attempt.
    ///
    /// The messages come from the caller. What you send is what you get, and whether the answer
    /// enters history is decided by whoever asked — which is what makes a verification turn
    /// possible without polluting the transcript.
    pub async fn inference(
        &self,
        session: &mut session::Session,
        messages: &[Message],
    ) -> Result<InferenceOutcome> {
        let routing = session.routing.clone();
        let targets = self.models.resolve(&routing)?;
        let call = self.call_model(session, messages, &targets).await?;
        session.steps = session.steps.saturating_add(1);
        let step_index = u32::try_from(session.steps).unwrap_or(u32::MAX);
        self.emit(
            session,
            EventKind::MessageCompleted {
                message: call.message.clone(),
                step_index,
            },
        )
        .await?;
        self.write_checkpoint(session).await?;
        Ok(InferenceOutcome {
            message: call.message,
            finish: call.finish,
            usage: session.usage.clone(),
        })
    }

    /// Ask policy about one call.
    pub async fn check(
        &self,
        session: &session::Session,
        call: &crate::contract::ToolCall,
    ) -> crate::adapter::Verdict {
        self.policy()
            .check(crate::adapter::PolicyRequest {
                principal: &session.principal,
                session: Some(&session.id),
                subject: crate::adapter::Gated::Tool { call },
            })
            .await
    }

    /// Run one tool call.
    ///
    /// Policy is consulted here as well as in [`Kernel::check`]: a kernel that executed an ungated
    /// tool because a caller forgot to ask first would not be a kernel.
    pub async fn invoke_tool(
        &self,
        session: &mut session::Session,
        call: &crate::contract::ToolCall,
    ) -> Result<ToolOutput> {
        let mut outputs = self
            .invoke_tools(session, std::slice::from_ref(call))
            .await?;
        outputs
            .pop()
            .ok_or_else(|| Error::internal("a single call produced no result"))
    }

    /// Run a batch of calls.
    ///
    /// Policy is asked about every call before any runs, so a batch that needs a human parks with
    /// nothing executed. Then the allowed ones run concurrently and their results are folded back
    /// in order — state writes serial, tool execution parallel, which are different axes.
    pub async fn invoke_tools(
        &self,
        session: &mut session::Session,
        calls: &[crate::contract::ToolCall],
    ) -> Result<Vec<ToolOutput>> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }

        let mut verdicts = Vec::with_capacity(calls.len());
        for call in calls {
            match self.check(session, call).await {
                crate::adapter::Verdict::AskHuman { question, options } => {
                    self.park(
                        session,
                        crate::contract::PendingCall {
                            call: call.clone(),
                            options,
                            question,
                        },
                    )
                    .await?;
                    return Err(Error::unavailable(
                        "policy asked a human; the batch is parked with nothing executed",
                    ));
                }
                crate::adapter::Verdict::Allow => verdicts.push(None),
                crate::adapter::Verdict::Deny { reason, .. } => verdicts.push(Some(reason)),
            }
        }
        self.execute_calls(session, calls, verdicts).await
    }

    /// Run calls that are already allowed.
    ///
    /// A human's verdict *is* the policy answer; asking again would ask the same question
    /// forever. This is the only door that skips the gate, and it is not reachable from the wire.
    pub async fn invoke_approved(
        &self,
        session: &mut session::Session,
        calls: &[crate::contract::ToolCall],
    ) -> Result<Vec<ToolOutput>> {
        let verdicts = vec![None; calls.len()];
        self.execute_calls(session, calls, verdicts).await
    }

    async fn execute_calls(
        &self,
        session: &mut session::Session,
        calls: &[crate::contract::ToolCall],
        verdicts: Vec<Option<String>>,
    ) -> Result<Vec<ToolOutput>> {
        let deadline = self.deadline(session);
        let session_id = session.id.clone();
        let started = self.now();
        let futures = calls.iter().zip(verdicts).map(|(call, denial)| {
            let tool = self.tool(&call.name);
            let args = call.arguments_json.clone();
            let call_id = call.id.clone();
            let session_id = session_id.clone();
            async move {
                let ctx = crate::adapter::ToolCtx {
                    session: &session_id,
                    call_id: &call_id,
                    deadline_unix_millis: deadline,
                };
                match denial {
                    None => match tool {
                        // A model that produced broken JSON gets told so, and the turn
                        // continues. Executing it would mean guessing at the arguments.
                        Some(tool) => match serde_json::from_str::<serde_json::Value>(&args) {
                            Ok(_) => tool.invoke(&args, &ctx).await,
                            Err(error) => ToolOutput::failed(format!(
                                "arguments were not valid JSON: {error}"
                            )),
                        },
                        None => ToolOutput::failed(format!("no tool named {:?}", ctx.call_id)),
                    },
                    Some(reason) => ToolOutput::failed(reason),
                }
            }
        });
        let outputs = futures_util::future::join_all(futures).await;
        let duration = self.now().saturating_sub(started);

        for (call, output) in calls.iter().zip(outputs.iter()) {
            self.record_call(session, call, output.clone(), duration)
                .await?;
        }
        self.write_checkpoint(session).await?;
        Ok(outputs)
    }

    /// Record a refusal as the call's result.
    ///
    /// A refusal is content, not a failure of the session: the model reads why and decides what
    /// to do next.
    pub async fn record_refusal(
        &self,
        session: &mut session::Session,
        call: &crate::contract::ToolCall,
        reason: impl Into<String>,
    ) -> Result<()> {
        self.record_call(session, call, ToolOutput::failed(reason), 0)
            .await
    }

    /// Record a call's outcome: the event, the spend, and the message that enters history.
    async fn record_call(
        &self,
        session: &mut session::Session,
        call: &crate::contract::ToolCall,
        output: ToolOutput,
        duration_millis: u64,
    ) -> Result<()> {
        // A tool that reported a cost is charged for it; one that said nothing is not assumed
        // free. The ledger's rule is the same one model usage follows.
        if let Some(usage) = &output.usage {
            session.usage.add(usage);
            session.credit_tokens = session.credit_tokens.saturating_sub(usage.total_tokens());
        }
        let result = crate::contract::ToolResult {
            call_id: call.id.clone(),
            content: output.content,
            is_error: output.is_error,
        };
        self.emit(
            session,
            EventKind::ToolFinished {
                duration_millis,
                result: result.clone(),
                usage: output.usage,
            },
        )
        .await?;
        session.messages.push(Message {
            role: crate::contract::Role::Tool,
            content: Content::new([Part::new(PartKind::ToolResult(ToolResultPart { result }))]),
        });
        Ok(())
    }

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

    /// The events a session has recorded, from `from_seq` onward.
    pub async fn events(&self, id: &SessionId, from_seq: u64) -> Result<Vec<Event>> {
        self.store.events_since(id, from_seq).await
    }

    /// The registered model names, sorted.
    #[must_use]
    pub fn models(&self) -> Vec<String> {
        self.models.names()
    }

    /// Resolve a route against the registry.
    pub fn resolve_route(&self, routing: &crate::contract::Routing) -> Result<ResolvedRoute> {
        self.models.resolve(routing)
    }

    /// One tool by name.
    #[must_use]
    pub fn tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// The declarations this session may use, in name order so a prompt is reproducible.
    #[must_use]
    pub fn tool_specs(&self, session: &session::Session) -> Vec<crate::contract::ToolSpec> {
        self.tools
            .iter()
            .filter(|(name, _)| session.tools.is_empty() || session.tools.contains(name))
            .map(|(_, tool)| tool.spec())
            .collect()
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
