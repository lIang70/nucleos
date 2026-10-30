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
    /// Caller-chosen id. A retried request with the same id resumes instead of creating a second
    /// session, which is the only way a `POST` survives a network retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Continue an existing session instead of creating one.
    #[serde(default)]
    pub resume: bool,
    /// Messages to seed the conversation with.
    #[serde(default)]
    pub initial: Vec<Message>,
    /// What the session may consume.
    #[serde(default)]
    pub budget: Budget,
    /// Tokens granted up front.
    #[serde(default)]
    pub credit_tokens: u64,
    /// Where commands run, and what they may touch.
    #[serde(default)]
    pub sandbox: SandboxSettings,
    /// The mutable configuration to start from — the same shape `configure` takes, so a session can
    /// begin in any state it could later be changed into.
    #[serde(default)]
    pub config: crate::contract::ConfigPatch,
}

impl StartRequest {
    /// The session id to use, derived from the route when the caller did not choose one.
    #[must_use]
    pub fn session_id(&self) -> String {
        self.session.clone().unwrap_or_else(|| {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            self.credit_tokens.hash(&mut hasher);
            self.initial.len().hash(&mut hasher);
            self.sandbox.workspace.hash(&mut hasher);
            format!("session-{:016x}", hasher.finish())
        })
    }
}

impl From<StartRequest> for crate::kernel::SessionSpec {
    fn from(request: StartRequest) -> Self {
        Self {
            // Chosen by the route, not by the body: `/v1/agents` opens an agent session and the
            // process route opens a process session.
            kind: crate::contract::SessionKind::Agent,
            process: None,
            workspace: request.sandbox.workspace.clone(),
            sandbox: request.sandbox.policy.clone(),
            initial: request.initial,
            credit_tokens: request.credit_tokens,
            budget: crate::contract::Budget {
                max_steps: request.budget.max_steps,
                max_tokens: request.budget.max_tokens,
                max_micros_usd: request.budget.max_micros_usd,
                max_wall_millis: request.budget.max_wall_millis,
            },
            config: request.config,
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
    /// Whether one turn may call several tools at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
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
    /// The tools a session may be given.
    #[serde(default)]
    pub tools: Vec<String>,
    /// The tool generation new sessions are pinned to.
    ///
    /// A reload increments it. Reported because "which set was the daemon on when this session
    /// started" is the first question asked when a session and a deployment disagree about what a
    /// tool accepts.
    #[serde(default)]
    pub generation: u64,
}

/// One model to add or replace.
///
/// No API key: `key_file` names a path. The system store is backed up and copied, and a store that
/// contains a credential turns every backup into a credential dump.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ModelRequest {
    /// The name a route uses.
    pub name: String,
    /// The responses endpoint, in full.
    pub url: String,
    /// The provider's model identifier.
    pub model: String,
    /// Where the API key is, if the endpoint wants one. Absent falls back to `NUCLEOS_MODEL_KEY`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<String>,
    /// The context window, when the deployment knows better than the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
}

/// A model, as the daemon reports it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ModelView {
    /// The name routes use.
    pub name: String,
    /// The endpoint.
    pub url: String,
    /// The provider's identifier.
    pub model: String,
    /// The context window in force.
    #[serde(default)]
    pub context_window: u32,
    /// Where its key is read from, if anywhere. A path, never the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<String>,
}

/// The models this deployment has.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ModelListResponse {
    /// In the order they were added.
    #[serde(default)]
    pub models: Vec<ModelView>,
}

/// What a change did.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct PutResponse {
    /// Which setting changed.
    pub changed: String,
    /// How many rules the new policy has, when it was a policy.
    #[serde(default)]
    pub rules: usize,
}

/// What this daemon is running.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ConfigResponse {
    /// The configuration schema this build writes.
    pub schema_version: u32,
    /// Where the data lives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// The policy document in force, as stored — the daemon does not summarise what it enforces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_json: Option<String>,
    /// The administrator keys enrolled.
    #[serde(default)]
    pub admin_keys: Vec<ConfigKey>,
    /// The tool generation new sessions are pinned to.
    #[serde(default)]
    pub generation: u64,
    /// How many bytes of body the signature covered.
    ///
    /// Zero for every read, which is the point: it shows the verifier ran over the request that was
    /// actually sent rather than over a body it assumed.
    #[serde(default)]
    pub signed_body_bytes: u64,
}

/// One enrolled administrator key, without the key material.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ConfigKey {
    /// The short identifier the signatures name.
    pub id: String,
    /// The tenant it acts for.
    pub tenant: String,
    /// The subject within it.
    pub subject: String,
}

/// What this daemon has counted since it started.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct MetricsResponse {
    /// `"GET /v1/describe"` → how many times, keyed by route template.
    #[serde(default)]
    pub requests: BTreeMap<String, u64>,
    /// Requests refused by authentication.
    #[serde(default)]
    pub unauthorized: u64,
    /// Sessions opened.
    #[serde(default)]
    pub sessions: u64,
    /// Tool sources reloaded.
    #[serde(default)]
    pub reloads: u64,
    /// Sessions that ended because a budget ran out.
    #[serde(default)]
    pub budget_stops: u64,
    /// How long the daemon has been counting.
    #[serde(default)]
    pub uptime_millis: u64,
}

/// What a tool reload did.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ReloadResponse {
    /// The generation new sessions are pinned to from now on.
    pub generation: u64,
    /// The tools it holds.
    pub tools: Vec<String>,
    /// What the reload added, by name.
    #[serde(default)]
    pub added: Vec<String>,
    /// What it removed, by name.
    ///
    /// A session pinned to the previous generation keeps working: `added` and `removed` describe
    /// what changed for sessions that start from now on, not what happened to sessions already
    /// running.
    #[serde(default)]
    pub removed: Vec<String>,
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

/// A command to start as a process session.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProcessStartRequest {
    /// The command line. No shell by default: an argv cannot be word-split by accident.
    pub argv: Vec<String>,
    /// Where to run it. Absent means the workspace below.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Environment to add.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// What to ask for on top of the session's policy.
    #[serde(default)]
    pub permissions: crate::contract::SandboxPermissions,
    /// What its commands may touch. Set here because a process session is created by this call.
    #[serde(default)]
    pub sandbox: SandboxSettings,
    /// A wall-clock ceiling, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_millis: Option<u64>,
}

/// One running process, as a listing sees it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProcessInfo {
    /// The session that owns it.
    pub session: String,
    /// The command, and where it is.
    pub process: crate::contract::ProcessState,
}

/// The processes this node knows about.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProcessListResponse {
    /// In session order.
    pub processes: Vec<ProcessInfo>,
    /// Where to continue. Absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_page_token: Option<String>,
}

/// What starting a process answered with.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProcessStartResponse {
    /// The session that owns it.
    pub session: String,
    /// The pid, when the platform gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Where it is now.
    pub state: crate::contract::ProcessStatus,
    /// The cursor a client reads its output from.
    pub next_seq: u64,
}

/// Bytes for a running process's stdin.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StdinRequest {
    /// What to write. Not necessarily a whole line.
    pub chunk: String,
}

/// What a `configure` call changed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ConfigureResponse {
    /// The fields that actually changed, in the order they were applied.
    pub changed: Vec<String>,
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
    /// Where it is, when it is a process session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<crate::contract::ProcessState>,
    /// What its commands may touch.
    pub sandbox: crate::contract::SandboxPolicy,
    /// Where they run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// What the assistant had produced when a turn was cut short.
    ///
    /// Absent for a session that has never been interrupted, which is almost every session. Present
    /// means a turn died mid-generation: the text was paid for and is often the useful part, and it
    /// is not in the conversation because a half-written message is not something the model should
    /// reason from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<crate::contract::PartialTurn>,
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
