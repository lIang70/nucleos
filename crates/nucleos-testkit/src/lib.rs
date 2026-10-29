//! Test doubles, for hosts and for this workspace's own tests.
//!
//! They exist so a kernel test can run without a network, a socket, a process or a sleeping
//! clock. They are a separate crate rather than a module behind a feature flag, so that nothing
//! here can reach a deployed binary: a fake in the production crate is one `use` away from being
//! the deployment.
//!
//! What is *not* here is as deliberate. A policy that allows everything is a real policy with a
//! real meaning, and it lives in `nucleos::adapter`; the store is SQLite, the same code path in a
//! test as in production, and it lives in `nucleos-sqlite`. Only things whose whole purpose is to
//! stand in for something real are in this crate.

use async_trait::async_trait;
use nucleos::adapter::{
    Clock, FinishReason, Model, ModelDelta, ModelRequest, ModelStream, Tool, ToolCtx, ToolOutput,
};
use nucleos::contract::{
    Attempt, Capability, Content, Part, PartKind, TextPart, ToolSpec,
};
use nucleos::error::Result;
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
    pub fn reporting_usage(mut self, usage: nucleos::contract::Usage) -> Self {
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
// helpers
// ---------------------------------------------------------------------------

/// A text content, for tests and for tools that answer in prose.
#[must_use]
pub fn text_content(text: impl Into<String>) -> Content {
    Content::new([Part::new(PartKind::Text(TextPart { text: text.into() }))])
}

/// A ledger holding one successful attempt, for tests.
#[must_use]
pub fn one_attempt(model: &str) -> nucleos::contract::Ledger {
    use nucleos::contract::{AttemptOutcome, AttemptPhase, AttemptTrigger, Usage};
    let mut ledger = nucleos::contract::Ledger::default();
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
