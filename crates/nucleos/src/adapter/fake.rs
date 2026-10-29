//! Fakes with no dependencies.
//!
//! They exist so every kernel test can run without a network, a socket, a process or a
//! sleeping clock — and so that a host has something to start from before it writes a real
//! adapter. They are part of the public API on purpose: a fake that only the kernel's own
//! tests can reach is a fake every host re-implements.

use crate::adapter::{
    Clock, FinishReason, Gated, Model, ModelDelta, ModelRequest, ModelStream, Policy,
    PolicyRequest, SessionPage, SessionQuery, Store, Tool, ToolCtx, ToolOutput, Verdict,
};
use crate::contract::{
    Attempt, Capability, Content, Event, Part, PartKind, SessionId, Snapshot, TextPart, ToolSpec,
};
use crate::error::{Error, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------

/// A clock that only moves when told to.
#[derive(Debug, Clone, Default)]
pub struct FixedClock(Arc<Mutex<u64>>);

impl FixedClock {
    /// Start at a given unix millisecond.
    #[must_use]
    pub fn at(millis: u64) -> Self {
        Self(Arc::new(Mutex::new(millis)))
    }

    /// Move forward.
    pub fn advance(&self, millis: u64) {
        let mut now = self.lock();
        *now = now.saturating_add(millis);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, u64> {
        // A poisoned lock means another thread panicked while holding it. Recovering is right
        // for a fake: the value is a plain integer with no invariant to preserve.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Clock for FixedClock {
    fn now_unix_millis(&self) -> u64 {
        *self.lock()
    }
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// The capability every fake advertises, unless a test overrides it.
fn fake_capability() -> Capability {
    Capability {
        tools: true,
        tool_choice_required: true,
        structured_output: true,
        streaming: true,
        context_window: 200_000,
        max_output_tokens: 8_192,
    }
}

/// A naive token estimate: four characters per token, rounded up.
///
/// Naive on purpose, and only ever used by fakes. A real adapter either has a tokenizer or
/// returns a conservative bound it is willing to defend; the kernel refuses to guess on the
/// adapter's behalf.
fn estimate_tokens(request: &ModelRequest) -> u64 {
    request.text_len().div_ceil(4).max(1)
}

/// A model that plays back a script, one entry per call.
///
/// An exhausted script answers with `Stop` and no content, so a runaway loop terminates
/// instead of hanging — a fake that streams forever turns a bug into a timeout.
#[derive(Debug, Clone)]
pub struct FakeModel {
    capability: Capability,
    state: Arc<Mutex<Scripted>>,
}

#[derive(Debug, Default)]
struct Scripted {
    remaining: Vec<Vec<ModelDelta>>,
    calls: usize,
    token_override: Option<u64>,
}

impl FakeModel {
    /// Stream text and then stop, once.
    #[must_use]
    pub fn text(chunks: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let deltas: Vec<ModelDelta> = chunks
            .into_iter()
            .map(|chunk| ModelDelta::Text {
                part_index: 0,
                text: chunk.into(),
            })
            .chain(std::iter::once(ModelDelta::Finished {
                reason: FinishReason::Stop,
            }))
            .collect();
        Self::with_script(vec![deltas])
    }

    /// Play one scripted answer per call, in order.
    #[must_use]
    pub fn with_script(script: Vec<Vec<ModelDelta>>) -> Self {
        Self {
            capability: fake_capability(),
            state: Arc::new(Mutex::new(Scripted {
                remaining: script,
                calls: 0,
                token_override: None,
            })),
        }
    }

    /// Advertise a different capability.
    #[must_use]
    pub fn with_capability(mut self, capability: Capability) -> Self {
        self.capability = capability;
        self
    }

    /// Report a fixed prompt-token count instead of estimating.
    #[must_use]
    pub fn with_token_count(self, tokens: u64) -> Self {
        self.lock().token_override = Some(tokens);
        self
    }

    /// How many calls were made. A driver's loop is the thing under test, so counting is
    /// usually the assertion.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.lock().calls
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Scripted> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl Model for FakeModel {
    fn capability(&self) -> Capability {
        self.capability
    }

    fn count_tokens(&self, request: &ModelRequest) -> Result<u64> {
        Ok(self
            .lock()
            .token_override
            .unwrap_or_else(|| estimate_tokens(request)))
    }

    async fn invoke(&self, _request: ModelRequest) -> Result<ModelStream> {
        let deltas = {
            let mut state = self.lock();
            state.calls = state.calls.saturating_add(1);
            if state.remaining.is_empty() {
                vec![ModelDelta::Finished {
                    reason: FinishReason::Stop,
                }]
            } else {
                state.remaining.remove(0)
            }
        };
        Ok(Box::pin(tokio_stream::iter(deltas.into_iter().map(Ok))))
    }
}

/// A model that answers every call with the same text.
#[derive(Debug, Clone)]
pub struct EchoModel {
    reply: String,
}

impl EchoModel {
    /// Reply with `reply` every time.
    #[must_use]
    pub fn new(reply: impl Into<String>) -> Self {
        Self {
            reply: reply.into(),
        }
    }
}

#[async_trait]
impl Model for EchoModel {
    fn capability(&self) -> Capability {
        fake_capability()
    }

    fn count_tokens(&self, request: &ModelRequest) -> Result<u64> {
        Ok(estimate_tokens(request))
    }

    async fn invoke(&self, _request: ModelRequest) -> Result<ModelStream> {
        let reply = self.reply.clone();
        Ok(Box::pin(tokio_stream::iter(vec![
            Ok(ModelDelta::Text {
                part_index: 0,
                text: reply,
            }),
            Ok(ModelDelta::Finished {
                reason: FinishReason::Stop,
            }),
        ])))
    }
}

/// Deltas for one complete tool call: start, one argument chunk, and a stop.
///
/// Arguments arrive as text even in the fake, because that is the shape the kernel assembles
/// and the shape a test needs to exercise.
#[must_use]
pub fn tool_call_deltas(id: &str, name: &str, arguments_json: &str) -> Vec<ModelDelta> {
    vec![
        ModelDelta::ToolCallStarted {
            index: 0,
            id: id.to_owned(),
            name: name.to_owned(),
        },
        ModelDelta::ToolCallArgs {
            index: 0,
            fragment: arguments_json.to_owned(),
        },
        ModelDelta::Finished {
            reason: FinishReason::ToolCalls,
        },
    ]
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

/// A tool that answers with a fixed value.
#[derive(Debug, Clone)]
pub struct FakeTool {
    spec: ToolSpec,
    output: ToolOutput,
}

impl FakeTool {
    /// Answer with text.
    #[must_use]
    pub fn new(name: impl Into<String>, answer: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            spec: ToolSpec {
                description: format!("the {name} tool"),
                name,
                input_schema_json: "{\"type\":\"object\"}".to_owned(),
            },
            output: ToolOutput::text(answer),
        }
    }

    /// Answer with a failure, which is content with `is_error` set.
    #[must_use]
    pub fn failing(mut self, answer: impl Into<String>) -> Self {
        self.output = ToolOutput::failed(answer);
        self
    }

    /// Report a cost for every call.
    #[must_use]
    pub fn reporting_usage(mut self, usage: crate::contract::Usage) -> Self {
        self.output = self.output.reporting_usage(usage);
        self
    }

    /// Replace the whole answer.
    #[must_use]
    pub fn answering(mut self, output: ToolOutput) -> Self {
        self.output = output;
        self
    }
}

#[async_trait]
impl Tool for FakeTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn invoke(&self, _args_json: &str, _cx: &ToolCtx<'_>) -> ToolOutput {
        self.output.clone()
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// An in-memory event log.
///
/// It enforces the same rules a durable store must: no gaps, no rewrites, and a snapshot that
/// never claims to include more than the log holds.
#[derive(Debug, Default, Clone)]
pub struct MemoryStore {
    inner: Arc<Mutex<MemoryState>>,
}

#[derive(Debug, Default)]
struct MemoryState {
    // Ordered, so a keyset cursor means something.
    events: BTreeMap<String, Vec<Event>>,
    snapshots: BTreeMap<String, Snapshot>,
}

impl MemoryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Store for MemoryStore {
    async fn append(&self, session: &SessionId, events: &[Event]) -> Result<()> {
        let mut state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let log = state.events.entry(session.as_str().to_owned()).or_default();
        let mut expected = u64::try_from(log.len()).unwrap_or(u64::MAX);
        for event in events {
            if event.session != *session {
                return Err(Error::invalid(format!(
                    "event for {} appended to {session}",
                    event.session
                )));
            }
            if event.seq != expected {
                return Err(Error::invalid(format!(
                    "event seq {} does not continue the log at {expected}",
                    event.seq
                )));
            }
            expected = expected.saturating_add(1);
        }
        log.extend_from_slice(events);
        Ok(())
    }

    async fn sessions(&self, query: &SessionQuery) -> Result<SessionPage> {
        let state = self.inner.lock().unwrap_or_else(|p| p.into_inner());

        // The kind lives in the checkpoint, which is where a real store would index it too.
        let matching: Vec<String> = state
            .events
            .keys()
            .filter(|session| match &query.after {
                Some(after) => session.as_str() > after.as_str(),
                None => true,
            })
            .filter(|session| match query.kind {
                Some(kind) => state
                    .snapshots
                    .get(session.as_str())
                    .is_some_and(|snapshot| snapshot.kind == kind),
                None => true,
            })
            .cloned()
            .collect();

        let sessions: Vec<SessionId> = matching
            .iter()
            .take(query.limit)
            .map(|session| SessionId::new(session.clone()))
            .collect();
        // The cursor is the last id *included*, and it means "continue strictly after this": a
        // cursor that points one past the page skips the row it names when a caller short-circuits
        // the page on its own.
        let next = (matching.len() > query.limit)
            .then(|| sessions.last().cloned())
            .flatten();

        Ok(SessionPage { sessions, next })
    }

    async fn snapshot(&self, session: &SessionId) -> Result<Option<Snapshot>> {
        let state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        Ok(state.snapshots.get(session.as_str()).cloned())
    }

    async fn write_snapshot(&self, snapshot: Snapshot) -> Result<()> {
        // Delegates so that the validation rule has exactly one implementation.
        self.store_snapshot(snapshot)
    }

    async fn events_since(&self, session: &SessionId, seq: u64) -> Result<Vec<Event>> {
        let state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        Ok(state
            .events
            .get(session.as_str())
            .map(|log| {
                log.iter()
                    .filter(|event| event.seq >= seq)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }
}

impl MemoryStore {
    /// Every event appended for a session. For assertions, not for the kernel.
    #[must_use]
    pub fn recorded(&self, session: &SessionId) -> Vec<Event> {
        let state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        state
            .events
            .get(session.as_str())
            .cloned()
            .unwrap_or_default()
    }

    fn store_snapshot(&self, snapshot: Snapshot) -> Result<()> {
        let mut state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let log_len = state
            .events
            .get(snapshot.session.as_str())
            .map_or(0, Vec::len);
        if usize::try_from(snapshot.seq).unwrap_or(usize::MAX) >= log_len {
            return Err(Error::invalid(format!(
                "snapshot claims seq {} but the log holds {log_len} events",
                snapshot.seq
            )));
        }
        state
            .snapshots
            .insert(snapshot.session.as_str().to_owned(), snapshot);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

/// A policy that allows everything.
///
/// The name is a warning: a deployment that never replaces this has no policy, and its
/// deny-by-default claim is a sentence in a document.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

#[async_trait]
impl Policy for AllowAll {
    async fn check(&self, _request: PolicyRequest<'_>) -> Verdict {
        Verdict::Allow
    }
}

/// A policy that refuses everything, for testing the refusal path.
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAll;

#[async_trait]
impl Policy for DenyAll {
    async fn check(&self, request: PolicyRequest<'_>) -> Verdict {
        let what = match request.subject {
            Gated::Tool { call } => call.name.clone(),
            Gated::Model { model, .. } => model.to_owned(),
            Gated::Exec { argv, .. } => argv.join(" "),
        };
        Verdict::Deny {
            code: crate::contract::ErrorCode::Denied,
            reason: format!("{what} is not available"),
        }
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// A text content, for tests and for tools that answer in prose.
#[must_use]
pub fn text_content(text: impl Into<String>) -> Content {
    Content::new([Part::new(PartKind::Text(TextPart { text: text.into() }))])
}

/// A ledger holding one successful attempt, for tests.
#[must_use]
pub fn one_attempt(model: &str) -> crate::contract::Ledger {
    use crate::contract::{AttemptOutcome, AttemptPhase, AttemptTrigger, Usage};
    let mut ledger = crate::contract::Ledger::default();
    ledger.record(Attempt {
        index: 0,
        model: model.to_owned(),
        phase: AttemptPhase::Execute,
        trigger: AttemptTrigger::Selection,
        outcome: AttemptOutcome::Ok,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 20,
            micros_usd: Some(30),
        },
        duration_millis: 5,
        degraded: Vec::new(),
    });
    ledger
}
