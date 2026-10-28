//! Fakes with no dependencies.
//!
//! They exist so every kernel test can run without a network, a socket, a process or a
//! sleeping clock — and so that a host has something to start from before it writes a real
//! adapter. They are part of the public API on purpose: a fake that only the kernel's own
//! tests can reach is a fake every adapter re-implements.

use crate::adapter::{
    Clock, FinishReason, Model, ModelDelta, ModelRequest, ModelStream, Policy, PolicyRequest,
    Store, Tool, ToolCtx, ToolOutput, Verdict,
};
use crate::contract::{
    Capability, Content, Event, Part, PartKind, SessionId, Snapshot, TextPart, ToolSpec,
};
use crate::error::{Error, Result};
use async_trait::async_trait;
use std::collections::HashMap;
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
        // A poisoned lock means another test thread panicked while holding it. Recovering is
        // right for a fake: the value is a plain integer and has no invariant to preserve.
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
            capability: Capability {
                tools: true,
                streaming: true,
                context_window: 200_000,
                max_output_tokens: 8_192,
            },
            state: Arc::new(Mutex::new(Scripted {
                remaining: script,
                calls: 0,
            })),
        }
    }

    /// How many calls were made. The engine's loop is the thing under test, so counting is
    /// usually the assertion.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.lock().calls
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Scripted> {
        // A poisoned lock means another thread panicked while holding it; the state is a
        // plain list and an integer, so recovering loses nothing.
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
        Capability {
            tools: true,
            streaming: true,
            context_window: 200_000,
            max_output_tokens: 8_192,
        }
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
    events: HashMap<String, Vec<Event>>,
    snapshots: HashMap<String, Snapshot>,
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

    async fn snapshot(&self, session: &SessionId) -> Result<Option<Snapshot>> {
        let state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        Ok(state.snapshots.get(session.as_str()).cloned())
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
    /// Record a snapshot. Not part of [`Store`]: only the engine writes one, and only at a
    /// turn boundary.
    pub fn write_snapshot(&self, snapshot: Snapshot) -> Result<()> {
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
        Verdict::Deny {
            code: crate::contract::ErrorCode::Denied,
            reason: format!("{} is not available", request.call.name),
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
