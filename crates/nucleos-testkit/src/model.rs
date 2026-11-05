//! Scripted models: one that follows a script, one that always answers the same thing.

use async_trait::async_trait;
use nucleos::adapter::{FinishReason, Model, ModelDelta, ModelRequest, ModelStream};
use nucleos::contract::Capability;
use nucleos::error::Result;
use std::sync::{Arc, Mutex};

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
