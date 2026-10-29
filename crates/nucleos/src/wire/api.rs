//! The envelopes that exist only at the edge.
//!
//! Requests and responses are not kernel types: the kernel has no `StartRequest`, it has a
//! `SessionSpec` and an `Outcome`. These structs are where the HTTP shape lives, and they are
//! part of the published schema because an SDK generates its client from it.

use crate::contract::{
    ErrorCode, Message, PendingCall, Purpose, SessionState, StopReason, ToolCall, ToolResult, Usage,
};
use crate::kernel::Outcome;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What a caller must send to create a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct StartRequest {
    /// Caller-chosen id. A retried request with the same id resumes instead of creating a
    /// second session, which is the only way a `POST` survives a network retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
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
    /// Where commands run, and what they may touch.
    #[serde(default)]
    pub sandbox: SandboxSettings,
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

impl StartRequest {
    /// The session id to use, derived from the route when the caller did not choose one.
    #[must_use]
    pub fn session_id(&self) -> String {
        self.session.clone().unwrap_or_else(|| {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            self.model_chain.hash(&mut hasher);
            self.system.hash(&mut hasher);
            self.credit_tokens.hash(&mut hasher);
            format!("session-{:016x}", hasher.finish())
        })
    }
}

impl From<StartRequest> for crate::kernel::SessionSpec {
    fn from(request: StartRequest) -> Self {
        use crate::adapter::{GenerateParams, ReasoningEffort, TextFormat, ToolChoice};
        Self {
            routing: request.model_chain.map(|chain| crate::contract::Routing {
                chain,
                retries: request.retries,
            }),
            system: request.system,
            initial: request.initial,
            tools: request.tools,
            params: GenerateParams {
                max_output_tokens: request.params.max_output_tokens,
                temperature: request.params.temperature,
                top_p: request.params.top_p,
                stop: request.params.stop,
                reasoning_effort: match request.params.reasoning_effort.as_deref() {
                    Some("minimal") => Some(ReasoningEffort::Minimal),
                    Some("low") => Some(ReasoningEffort::Low),
                    Some("medium") => Some(ReasoningEffort::Medium),
                    Some("high") => Some(ReasoningEffort::High),
                    _ => None,
                },
                tool_choice: match request.params.tool_choice.as_deref() {
                    Some("required") => ToolChoice::Required,
                    Some("none") => ToolChoice::None,
                    _ => ToolChoice::Auto,
                },
                format: match request.params.format {
                    None | Some(Format::Text) => TextFormat::Text,
                    Some(Format::JsonObject) => TextFormat::JsonObject,
                    Some(Format::JsonSchema { name, schema }) => TextFormat::JsonSchema {
                        name,
                        schema_json: schema.to_string(),
                    },
                },
            },
            budget: crate::contract::Budget {
                max_steps: request.budget.max_steps,
                max_tokens: request.budget.max_tokens,
                max_micros_usd: request.budget.max_micros_usd,
                max_wall_millis: request.budget.max_wall_millis,
            },
            credit_tokens: request.credit_tokens,
            // The policy is the operator's, not the caller's. A request that could name its own
            // sandbox could name `danger_full_access`.
            workspace: request.sandbox.workspace.clone(),
            sandbox: request.sandbox.policy.clone(),
            // Chosen by the route, not by the body: `/v1/agents` opens an agent session and the
            // process route opens a process session.
            kind: crate::contract::SessionKind::Agent,
            process: None,
        }
    }
}

/// The sandbox a session's commands run in.
///
/// Config, not request: it is decided when the session opens and carried in its checkpoint, so a
/// single command cannot widen it without asking.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SandboxSettings {
    /// Where commands run. Absent means the daemon's working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// What they may touch.
    #[serde(default)]
    pub policy: crate::contract::SandboxPolicy,
}

/// Generation parameters, as the wire carries them.
///
/// Mirrors `crate::adapter::GenerateParams`, but as a `#[serde(default)]`-friendly struct:
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

/// What a caller learns before it starts anything.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DescribeResponse {
    /// The daemon's version.
    pub version: String,
    /// The ABI version it speaks, from the schema's own `$id`.
    pub abi_version: String,
    /// The models a route may name.
    pub models: Vec<String>,
}

/// One inference: the messages to send.
///
/// The messages are the caller's, not the session's. What you send is what you get, and whether
/// the answer enters history is the next request's business — which is what makes a verification
/// turn possible without polluting the transcript.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct InferenceRequest {
    /// The conversation to send, oldest first. Absent sends nothing.
    #[serde(default)]
    pub messages: Vec<Message>,
}

/// What one inference produced.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InferenceResponse {
    /// The assistant message. Nothing has been appended to the session.
    pub message: Message,
    /// The provider's own reason for stopping.
    pub finish: String,
    /// What the session has spent so far.
    pub usage: Usage,
}

/// A batch of tool calls.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ToolBatchRequest {
    /// The calls to run. Policy is asked about all of them before any runs.
    #[serde(default)]
    pub calls: Vec<ToolCall>,
}

/// What the batch produced.
///
/// A failure is a result with `is_error`, not an HTTP error: the model reads why and decides.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ToolBatchResponse {
    /// One result per call, in order.
    pub results: Vec<ToolResult>,
}

/// A write to the session's own state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct StateRequest {
    /// Messages to append to the conversation.
    #[serde(default)]
    pub messages: Vec<Message>,
    /// Keys to set in the application's opaque state.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub data: BTreeMap<String, serde_json::Value>,
}

/// One command to run inside the session's sandbox.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ExecRequest {
    /// The command line. No shell by default: an argv cannot be word-split by accident.
    pub argv: Vec<String>,
    /// Where to run it. Absent means the session's workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Environment to add.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Wall-clock ceiling. Absent means the session's remaining budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_millis: Option<u64>,
    /// What to ask for on top of the session's policy.
    #[serde(default)]
    pub permissions: crate::contract::SandboxPermissions,
}

/// What a session looks like from outside.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionView {
    /// Its identifier.
    pub session: String,
    /// Whether it still takes calls.
    pub state: SessionState,
    /// The cursor a client resumes from.
    pub next_seq: u64,
    /// How many inferences it has made.
    pub inference_calls: u64,
    /// Calls waiting on a human.
    pub pending: Vec<PendingCall>,
    /// What it has spent.
    pub usage: Usage,
    /// What its commands may touch.
    pub sandbox: crate::contract::SandboxPolicy,
    /// Where they run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
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
