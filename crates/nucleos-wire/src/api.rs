//! The envelopes that exist only at the edge.
//!
//! Requests and responses are not kernel types: the kernel has no `StartRequest`, it has a
//! `SessionSpec` and an `Outcome`. These structs are where the HTTP shape lives, and they are
//! part of the published schema because an SDK generates its client from it.

use nucleos::contract::{ErrorCode, Message, Purpose, SessionState, StopReason};
use nucleos::engine::Outcome;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What a caller must send to create a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct StartRequest {
    /// The route, first name that answers wins. Absent asks for the daemon's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_chain: Option<Vec<String>>,
    /// Extra attempts per target before falling through.
    #[serde(default)]
    pub retries: u32,
    /// Standing instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// The opening messages.
    #[serde(default)]
    pub initial: Vec<Message>,
    /// Tool names this session may use. Empty means every registered tool.
    #[serde(default)]
    pub tools: Vec<String>,
    /// How to shape answers.
    #[serde(default)]
    pub params: Params,
    /// What the session may consume.
    #[serde(default)]
    pub budget: Budget,
    /// Tokens granted up front, spent by usage. Zero means "no credit, no model calls".
    #[serde(default)]
    pub credit_tokens: u64,
    /// Continue an existing session instead of creating one.
    #[serde(default)]
    pub resume: bool,
}

/// Generation parameters, as the wire carries them.
///
/// Mirrors `nucleos::adapter::GenerateParams`, but as a `#[serde(default)]`-friendly struct:
/// a caller should be able to send `{}` and get the defaults, which is not true of the kernel's
/// type because its `TextFormat` is an enum.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Params {
    /// The completion ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Nucleus sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Sequences that end the completion.
    #[serde(default)]
    pub stop: Vec<String>,
    /// Reasoning effort, for models that expose one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// `auto`, `required` or `none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,
    /// `text`, `json_object`, or a JSON Schema object to conform to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<Format>,
}

/// The shape the answer must have.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Format {
    /// Prose.
    Text,
    /// A JSON object, schema unchecked.
    JsonObject,
    /// A JSON object that must match the schema.
    JsonSchema {
        /// The name the provider files it under.
        name: String,
        /// The schema, as a JSON object.
        schema: serde_json::Value,
    },
}

/// What a session may consume.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Budget {
    /// Turns a session may take.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u64>,
    /// Tokens across every model call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Cost in micro-USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_micros_usd: Option<u64>,
    /// Wall clock in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_wall_millis: Option<u64>,
}

/// What a caller gets back after starting or resuming.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StartResponse {
    /// Which session.
    pub session: String,
    /// Where it is.
    pub state: SessionState,
    /// Why it stopped, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopReason>,
    /// The cursor a client resumes from.
    pub next_seq: u64,
}

impl From<&Outcome> for StartResponse {
    fn from(outcome: &Outcome) -> Self {
        Self {
            session: outcome.session.as_str().to_owned(),
            state: outcome.state,
            stop: outcome.stop,
            next_seq: outcome.next_seq,
        }
    }
}

/// What a caller sends to answer a parked call.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct VerdictRequest {
    /// Which call was gated.
    pub tool_call_id: String,
    /// Which offered option was picked.
    pub option_id: String,
}

/// The body of a failed request.
///
/// One shape for every failure: a caller should not have to know which endpoint it called to
/// know how to read why it failed.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ErrorBody {
    /// A stable category programs match on.
    pub code: ErrorCode,
    /// For humans. Never parsed.
    pub reason: String,
    /// A statement about this request, not about the world.
    pub retryable: bool,
    /// When to retry, in milliseconds. Absent means no hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_millis: Option<u64>,
    /// Machine-readable detail, e.g. `{"needed_tokens": "40000"}`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub details: std::collections::BTreeMap<String, String>,
}

/// Why a call is happening, on the wire.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PurposeField {
    /// The answer the caller waits for.
    Answer,
    /// The kernel's own bookkeeping.
    Summary,
    /// Another try at an unusable turn.
    Repair,
}

impl From<Purpose> for PurposeField {
    fn from(purpose: Purpose) -> Self {
        match purpose {
            Purpose::Answer => Self::Answer,
            Purpose::Summary => Self::Summary,
            Purpose::Repair => Self::Repair,
        }
    }
}
