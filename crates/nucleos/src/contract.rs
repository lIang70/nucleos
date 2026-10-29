//! The kernel's pure types.
//!
//! No I/O, no clocks, no sockets. Everything here is data that can be compared, logged,
//! stored and replayed, which is what lets a driver be tested without a runtime and an
//! event log be replayed without a network.
//!
//! The types are deliberately narrower than any provider's vocabulary. A capability that
//! has no representation here is a capability the kernel does not claim to understand.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// A session identifier.
///
/// The caller chooses it when resuming, so it is a string rather than an opaque handle.
/// That makes a retried `Start` idempotent instead of creating a second session.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub struct SessionId(String);

impl SessionId {
    /// Wrap a caller-supplied identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A tool-source generation.
///
/// Generations are immutable snapshots of the declared sources. A session is pinned to
/// one at start and may only move forward at a turn boundary, and only when the two
/// generations are compatible.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
)]
pub struct Generation(pub u64);

// ---------------------------------------------------------------------------
// The content model
// ---------------------------------------------------------------------------

/// Who produced a message.
///
/// The set is the smallest one every provider can express. A provider with a fifth role
/// maps it onto one of these in its adapter — the kernel does not learn it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Instructions.
    System,
    /// The caller.
    User,
    /// The model.
    Assistant,
    /// A tool's output.
    Tool,
}

/// One turn's worth of content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Message {
    /// Who produced it.
    pub role: Role,
    /// What it contains.
    ///
    /// Non-empty whenever the kernel writes one: an empty content is a bug in whatever
    /// produced it, not something a reader has to tolerate.
    pub content: Content,
}

/// An ordered multimodal payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Content {
    /// The parts, in order.
    pub parts: Vec<Part>,
}

impl Content {
    /// Build a content from parts.
    #[must_use]
    pub fn new(parts: impl IntoIterator<Item = Part>) -> Self {
        Self {
            parts: parts.into_iter().collect(),
        }
    }

    /// Concatenate every text part. Thinking, images and tool parts are skipped.
    ///
    /// One definition, because "the text of a content" drifting into "first text part" for
    /// one caller and "all text parts joined" for another is how two readers of the same
    /// value start disagreeing.
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = String::new();
        for part in &self.parts {
            if let PartKind::Text(text) = &part.kind {
                out.push_str(&text.text);
            }
        }
        out
    }

    /// Whether there is nothing to render.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }
}

/// One piece of content.
///
/// The union is small on purpose: seven kinds. Audio and video are [`PartKind::File`] with an
/// `audio/*` or `video/*` media type, because the kernel moves those bytes and never
/// interprets them. A kind is added when the kernel must act differently — not when a
/// provider can emit something new.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Part {
    /// What it is.
    #[serde(flatten)]
    pub kind: PartKind,
    /// Contextual or vendor-specific data.
    ///
    /// The kernel never reads it, never interprets it, and never rejects an unknown key.
    /// This is the only extension hatch: a schema with two of them has everyone using the
    /// other one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, String>,
}

impl Part {
    /// A part with no extension data — the common case.
    #[must_use]
    pub fn new(kind: PartKind) -> Self {
        Self {
            kind,
            meta: BTreeMap::new(),
        }
    }
}

/// The kind of a [`Part`], and its payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PartKind {
    /// Plain text.
    Text(TextPart),
    /// An image, inline or referenced.
    Image(ImagePart),
    /// A reference to content that is not carried here.
    File(FilePart),
    /// Structured output.
    Data(DataPart),
    /// The model asking for a tool.
    ToolCall(ToolCallPart),
    /// A tool's answer, folded back into history.
    ToolResult(ToolResultPart),
    /// A provider reasoning trace.
    Reasoning(ReasoningPart),
}

/// Plain text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TextPart {
    /// The text.
    pub text: String,
}

/// An image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ImagePart {
    /// Where the bytes are.
    pub source: MediaSource,
}

/// Bytes, or a reference to them, plus the type that makes them renderable.
///
/// `url` exists because inlining a large image into an event log is a storage incident: the
/// log is append-only and replayed, so every byte is paid for on every replay. `data` exists
/// because a caller with no reachable store must still be able to send one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MediaSource {
    /// The media type, e.g. `image/png`. Required: bytes without a type are not renderable.
    pub media_type: String,
    /// Where the bytes come from.
    pub from: MediaFrom,
}

/// Where media bytes live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum MediaFrom {
    /// A URL to fetch.
    Url {
        /// The URL.
        url: String,
    },
    /// Bytes carried inline.
    Data {
        /// The bytes.
        data: Vec<u8>,
    },
}

/// A reference to content that is not carried here: an artifact, a workspace file, or a
/// truncated tool result's spill.
///
/// This is also how truncation is expressed. A result too large to store is spilled; the
/// text keeps a marker and this part keeps the pointer. That is a policy outcome, not an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FilePart {
    /// Where to find it.
    pub uri: String,
    /// What it is.
    pub media_type: String,
    /// A human-facing name.
    pub name: String,
}

/// Structured output. JSON text rather than bytes, because JSON is UTF-8 by definition and
/// text is what every toolchain can read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DataPart {
    /// The media type, usually `application/json`.
    pub media_type: String,
    /// The JSON, as text.
    pub json: String,
}

/// The model asking for a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCallPart {
    /// The call.
    pub call: ToolCall,
}

/// A tool's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolResultPart {
    /// The result.
    pub result: ToolResult,
}

/// One tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCall {
    /// The correlation id.
    pub id: String,
    /// The tool name.
    pub name: String,
    /// A JSON object, kept as text so that a replay is byte-faithful.
    pub arguments_json: String,
    /// Opaque extension, same rule as [`Part::meta`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, String>,
}

/// How a tool call ended.
///
/// **Failure is a flag, not a branch.** A failed call still carries content, because the
/// error text is something the model must be able to read and reason about; putting the
/// failure outside the content model would leave the model with a message it cannot see.
/// The cost is honest: `is_error` is a runtime flag a client can forget to check, where a
/// `oneof` would have been enforced by the type system.
///
/// One rule constrains the recursion `Content → Part → ToolResult → Content` otherwise
/// allows: a tool result's content may contain any kind **except** [`PartKind::ToolCall`] or
/// [`PartKind::ToolResult`]. Nesting is one level deep, and a validator enforces it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolResult {
    /// The call this answers.
    pub call_id: String,
    /// What came back — on success and on failure alike.
    pub content: Content,
    /// Whether the call failed.
    pub is_error: bool,
}

/// One provider reasoning trace.
///
/// `signature` is why this is not a plain string: some providers sign a reasoning block and
/// reject the next turn when the signature is not returned verbatim. Dropping it turns a
/// multi-turn session into a failure on turn two. `id` and `source` exist for the same
/// reason — they are opaque to the kernel and must round-trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReasoningPart {
    /// The reasoning text. May be empty when the provider withholds it.
    pub text: String,
    /// The provider's signature, returned verbatim.
    pub signature: String,
    /// The provider's item id, when it uses one.
    pub id: String,
    /// The provider's verification scope.
    pub source: String,
}

// ---------------------------------------------------------------------------
// Session state
// ---------------------------------------------------------------------------

/// Where a session is in its life.
///
/// The vocabulary is the session's, not a provider's. `AwaitingHuman` is a state rather
/// than a callback, so a confirmation that arrives three hours later resumes a session
/// instead of reviving a stack frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Accepting calls.
    Open,
    /// Finished. No further call is accepted.
    Closed,
}

impl SessionState {
    /// Whether the session accepts no further calls.
    #[must_use]
    pub fn is_closed(self) -> bool {
        matches!(self, Self::Closed)
    }
}

/// Why a session stopped.
///
/// Four layers, all present: a provider's `finish_reason`, the step cap, the wall clock,
/// and the cost ledger. Each alone can be defeated; together they cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model said it was finished and no stop condition fired first.
    Completed,
    /// The step cap was reached.
    Steps,
    /// The token budget was spent.
    Tokens,
    /// The cost budget was spent.
    Cost,
    /// The wall-clock budget was spent.
    Wall,
    /// Policy denied a call.
    Denied,
    /// A tool or model call failed.
    Failed,
    /// Stopped on request.
    Cancelled,
}

// ---------------------------------------------------------------------------
// Accounting
// ---------------------------------------------------------------------------

/// What a session may consume.
///
/// `None` means unbounded for that dimension, which is a decision the host makes, not a
/// default the kernel assumes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Budget {
    /// Turns a session may take.
    pub max_steps: Option<u64>,
    /// Tokens across every model call, including failed attempts that streamed.
    pub max_tokens: Option<u64>,
    /// Cost in micro-USD. Integer, because money in a float is a rounding argument waiting
    /// to happen.
    pub max_micros_usd: Option<u64>,
    /// Wall clock for the whole session.
    pub max_wall_millis: Option<u64>,
}

/// What a model call consumed.
///
/// A counter the provider did not report stays `None` rather than becoming zero: a budget
/// that charges for silence is worse than one that knows it does not know.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Usage {
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Completion tokens.
    pub output_tokens: u64,
    /// Cost in micro-USD, when the provider reported one.
    pub micros_usd: Option<u64>,
}

impl Usage {
    /// Fold another report in.
    ///
    /// An unreported cost stays unreported. Adding an unknown as zero under-reports the
    /// bill, which is the one direction nobody forgives.
    pub fn add(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.micros_usd = match (self.micros_usd, other.micros_usd) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            _ => None,
        };
    }

    /// Tokens billed, however the provider split them.
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// An append-only record of something that happened.
///
/// `seq` is dense and gap-free, which is what makes it usable as a resume cursor without
/// a separate acknowledgement protocol. `schema_version` is written alongside the event so
/// a reader can upcast instead of guessing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Event {
    /// Monotonic within a session, starting at 0.
    pub seq: u64,
    /// The session this belongs to.
    pub session: SessionId,
    /// Unix milliseconds, from the injected clock so replay is deterministic.
    pub at_unix_millis: u64,
    /// The schema this event was written with.
    pub schema_version: u32,
    /// What happened.
    pub kind: EventKind,
    /// Opaque extension, same rule as [`Part::meta`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, String>,
}

impl Event {
    /// The schema version written by this build.
    pub const SCHEMA_VERSION: u32 = 1;
}

/// The payload of an [`Event`].
///
/// Events carry the content model rather than copies of its fields — [`ToolCall`] here is
/// the same type a [`ToolCallPart`] holds — so a live stream and a stored history cannot
/// disagree about the shape of an answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    /// A session state transition, with the reason when it is terminal.
    StateChanged {
        /// The state entered.
        state: SessionState,
        /// Set when the state is terminal.
        stop: Option<StopReason>,
    },
    /// A fragment of assistant text.
    TextDelta {
        /// Which part of the message it belongs to.
        ///
        /// Without it a client cannot tell a second text block from a continuation of the
        /// first, so text → tool call → more text is unrepresentable in the stream.
        part_index: u32,
        /// The fragment.
        text: String,
    },
    /// A fragment of a reasoning trace.
    ///
    /// `signature` usually arrives on its own, in a delta with empty `text`: a provider
    /// signs the finished block rather than each fragment.
    ReasoningDelta {
        /// Which part of the message it belongs to.
        part_index: u32,
        /// The fragment.
        text: String,
        /// The provider's signature fragment, returned verbatim.
        signature: String,
    },
    /// A tool call began. Its arguments follow as [`EventKind::ToolCallArgs`].
    ToolCallStarted {
        /// Position in this turn's tool-call list.
        index: u32,
        /// The correlation id the result will be filed under.
        tool_call_id: String,
        /// The tool name.
        name: String,
        /// Wall-clock deadline for this call.
        deadline_unix_millis: u64,
        /// When set, the tool reports progress under this token.
        progress_token: String,
    },
    /// A fragment of one call's arguments.
    ///
    /// Arguments stream as text because that is what providers send and because a partial
    /// object cannot be handed to a tool. The kernel assembles and validates before anything
    /// runs; malformed arguments become a result the model can read.
    ToolCallArgs {
        /// Which call this fragment belongs to.
        index: u32,
        /// The fragment.
        fragment: String,
    },
    /// A tool call resolved.
    ToolFinished {
        /// How long it took.
        duration_millis: u64,
        /// The result — the same value that enters history.
        result: ToolResult,
        /// What the tool reported it spent, when it reported anything.
        ///
        /// Absent is not zero: a tool that says nothing has not said it was free.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    /// One model call's consumption, debited whether or not the answer was usable.
    Usage {
        /// What it consumed.
        usage: Usage,
        /// Why the call happened, so a summary's spend can be told from an answer's.
        purpose: Purpose,
        /// The model that served it, after routing.
        model: String,
        /// Which attempt in the ledger it belongs to.
        attempt_index: u32,
    },
    /// The session's mutable configuration changed.
    ///
    /// Recorded because a replay has to know which system prompt was in force at turn 42; without
    /// it, the log would rebuild a conversation that never happened.
    SessionConfigured {
        /// What changed, field by field.
        patch: ConfigPatch,
    },
    /// A field the provider could not honour.
    ///
    /// Reported, never swallowed: a `tool_choice = Required` that quietly became `Auto` is a
    /// session behaving differently than it was configured to.
    CapabilityDegraded(Degraded),
    /// One attempt ended badly. Recorded because a retry does not erase the attempt that failed,
    /// or the money it spent.
    AttemptFailed {
        /// What happened.
        attempt: Attempt,
    },
    /// A policy verdict of `AskHuman` was reached.
    AwaitingHuman {
        /// What the host should ask.
        question: String,
        /// The call being gated.
        tool_call_id: String,
        /// Typed answers, so a client can render "once" and "always" differently and the
        /// verdict can say which was chosen.
        options: Vec<PermissionOption>,
        /// What the kernel does if nobody answers. A refusal, not a silent approval.
        default_if_unanswered: PermissionKind,
    },
    /// A budget dimension ran out.
    BudgetExhausted {
        /// Which one.
        reason: StopReason,
    },
    /// Something failed.
    Failed {
        /// A stable code for programs.
        code: ErrorCode,
        /// For humans.
        reason: String,
    },
    /// The session moved to a newer tool-source generation.
    ///
    /// Recorded rather than inferred, so a replay reproduces which tool set each turn ran
    /// against.
    GenerationMigrated {
        /// Where it came from.
        from: Generation,
        /// Where it went.
        to: Generation,
    },
    /// The assembled message a model call produced.
    ///
    /// Deltas are a rendering protocol; this is the record. Some content can only be
    /// assembled — a reasoning signature arrives as its own delta — and a client that joins
    /// late should render without replaying every fragment.
    MessageCompleted {
        /// The message, as it enters history.
        message: Message,
        /// Which step of the turn produced it.
        step_index: u32,
    },
    /// A command started.
    ExecStarted {
        /// The process id, when the platform gave one.
        pid: u32,
        /// What was run, verbatim, so an audit can see it.
        argv: Vec<String>,
        /// The policy it ran under, after any granted widening.
        policy: SandboxPolicy,
    },
    /// A chunk of a command's output.
    ExecOutput {
        /// Which stream.
        stream: ExecStream,
        /// The text of this chunk.
        chunk: String,
    },
    /// Bytes written to a running process's stdin.
    ///
    /// Recorded because it is the most audit-worthy thing in the system: spawning `/bin/sh` is
    /// safe, writing `rm -rf /` into it is not.
    ExecStdin {
        /// What was written.
        chunk: String,
    },
    /// A command ended.
    ExecExited {
        /// How it ended.
        output: ExecOutput,
    },
    /// Progress from a long-running call. A call whose only feedback is a deadline is
    /// indistinguishable from a hang.
    Progress {
        /// The token from [`EventKind::ToolCallStarted`].
        token: String,
        /// Units completed.
        done: u64,
        /// Units expected. Zero means unknown, not zero.
        total: u64,
        /// For humans.
        message: String,
    },
}

impl EventKind {
    /// The routable type string, e.g. `nucleos.v1.tool_finished`.
    ///
    /// The wire carries this alongside the `oneof` on purpose: a proxy filters on it
    /// without compiling the schema. Deriving it here rather than storing it means the two
    /// can never disagree.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::StateChanged { .. } => "nucleos.v1.state_changed",
            Self::TextDelta { .. } => "nucleos.v1.text_delta",
            Self::ReasoningDelta { .. } => "nucleos.v1.reasoning_delta",
            Self::ToolCallStarted { .. } => "nucleos.v1.tool_call_started",
            Self::ToolFinished { .. } => "nucleos.v1.tool_finished",
            Self::ToolCallArgs { .. } => "nucleos.v1.tool_call_args",
            Self::Usage { .. } => "nucleos.v1.usage",
            Self::ExecStarted { .. } => "nucleos.v1.exec_started",
            Self::ExecOutput { .. } => "nucleos.v1.exec_output",
            Self::ExecStdin { .. } => "nucleos.v1.exec_stdin",
            Self::ExecExited { .. } => "nucleos.v1.exec_exited",
            Self::SessionConfigured { .. } => "nucleos.v1.session_configured",
            Self::CapabilityDegraded(_) => "nucleos.v1.capability_degraded",
            Self::AttemptFailed { .. } => "nucleos.v1.attempt_failed",
            Self::AwaitingHuman { .. } => "nucleos.v1.awaiting_human",
            Self::BudgetExhausted { .. } => "nucleos.v1.budget_exhausted",
            Self::Failed { .. } => "nucleos.v1.failed",
            Self::GenerationMigrated { .. } => "nucleos.v1.generation_migrated",
            Self::MessageCompleted { .. } => "nucleos.v1.message_completed",
            Self::Progress { .. } => "nucleos.v1.progress",
        }
    }
}

/// What a human may answer when a call is gated.
///
/// The kernel *reports* the choice; it never decides that "always" applies. That is policy,
/// and it belongs to the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionOption {
    /// Caller-chosen, returned verbatim in the verdict.
    pub id: String,
    /// What it means.
    pub kind: PermissionKind,
    /// For humans.
    pub label: String,
}

/// The kinds of answer a permission request offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    /// Allow this call.
    AllowOnce,
    /// Allow this call and, if the host agrees, this tool for the session.
    AllowAlways,
    /// Refuse this call.
    Reject,
}

/// A stable error category.
///
/// Codes are enums rather than strings because the first caller who compares a string
/// freezes it forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request was malformed.
    Invalid,
    /// The thing named does not exist.
    NotFound,
    /// Policy refused.
    Denied,
    /// A budget was exhausted.
    Budget,
    /// A deadline passed.
    Timeout,
    /// A dependency was not reachable.
    Unavailable,
    /// A bug.
    Internal,
}

impl ErrorCode {
    /// Whether retrying the same request could succeed.
    ///
    /// A statement about the request, not about the world: `Budget` is retryable only
    /// after the caller grants more credit.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::Unavailable | Self::Timeout)
    }
}

// ---------------------------------------------------------------------------
// Kernel-facing types
// ---------------------------------------------------------------------------

/// Who is asking.
///
/// Travels as data. The kernel never authenticates, never verifies a signature, and never
/// derives a tenant from a request field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Principal {
    /// The tenant this caller belongs to.
    pub tenant: String,
    /// The subject within it.
    pub subject: String,
}

/// A tool, declared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolSpec {
    /// The name the model calls.
    pub name: String,
    /// What it does, in the model's words.
    pub description: String,
    /// A JSON Schema object, kept as text so the kernel never interprets it.
    pub input_schema_json: String,
}

/// What a model can do.
///
/// Checked at `Start`, never discovered by trial: a model that silently ignores tools, or
/// silently truncates a prompt, changes the answer without saying so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Capability {
    /// Whether it accepts tool definitions and emits tool calls.
    pub tools: bool,
    /// Whether it can be *made* to call one. A provider that accepts tools but ignores
    /// `tool_choice = required` turns a configured guarantee into a suggestion.
    pub tool_choice_required: bool,
    /// Whether it can be held to a JSON Schema.
    pub structured_output: bool,
    /// Whether [`crate::adapter::Model::invoke`] streams, or returns one final delta.
    pub streaming: bool,
    /// Total context, prompt plus completion.
    pub context_window: u32,
    /// The largest completion the provider will produce.
    pub max_output_tokens: u32,
}

/// How strongly a tool's execution is separated from the kernel.
///
/// Declared per tool source and enforced by whoever launches it. The kernel only refuses to
/// migrate a session into a generation where it changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// Trusted in-process code; nothing to enforce.
    None,
    /// A child process, confined by the OS.
    Process,
    /// A virtual machine or a wasm instance.
    Vm,
}

/// A call waiting on a human.
///
/// Part of the snapshot because the wait outlives any process: a session parked on a
/// confirmation must be resumable tomorrow, and the call it is holding has to come back with
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PendingCall {
    /// The call that has not run.
    pub call: ToolCall,
    /// What may be answered.
    pub options: Vec<PermissionOption>,
    /// What was asked.
    pub question: String,
}

/// A session's state, materialised at a point in its log.
///
/// `seq` is the event this snapshot already includes, so recovery is `load(snapshot)` then
/// `events_since(snapshot.seq + 1)`. Two numbers that must agree, written down once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Snapshot {
    /// The session it belongs to.
    pub session: SessionId,
    /// The last event folded in.
    pub seq: u64,
    /// The tool-source generation the session is pinned to.
    pub generation: Generation,
    /// Where the session was.
    pub state: SessionState,
    /// The conversation as of `seq`.
    pub messages: Vec<Message>,
    /// What it has spent.
    pub usage: Usage,
    /// How many inference calls it has made.
    pub inference_calls: u64,
    /// Tokens granted and not yet spent.
    ///
    /// In the checkpoint because every syscall may be served by a different process: a budget
    /// that resets on reload is not a budget.
    pub credit_tokens: u64,
    /// Calls waiting on a human. A batch can have more than one.
    #[serde(default)]
    pub pending: Vec<PendingCall>,
    /// The route this session was started with.
    ///
    /// In the checkpoint because a resume that had to be told its own route again is a resume
    /// that can silently behave differently — and the caller cannot know what to send.
    pub routing: Routing,
    /// Standing instructions.
    pub system: Option<String>,
    /// Tool names this session may use.
    pub tools: Vec<String>,
    /// How answers are shaped.
    pub params: GenerateParams,
    /// Where the session's commands run, and the default writable root.
    pub workspace: Option<String>,
    /// What its commands may touch.
    pub sandbox: SandboxPolicy,
    /// The adapter hints in force.
    #[serde(default)]
    pub hints: Hints,
    /// The application's own state.
    ///
    /// Opaque: the kernel stores it, updates it atomically and restores it, and never reads a
    /// key. Every write is also an event, so a replay reproduces the value the application saw.
    #[serde(default)]
    pub data: BTreeMap<String, serde_json::Value>,
    /// What this session is for.
    #[serde(default)]
    pub kind: SessionKind,
    /// The process, when this is a process session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcessState>,
}

// ---------------------------------------------------------------------------
// Route and ledger
// ---------------------------------------------------------------------------

/// Why a model call is happening.
///
/// The kernel makes calls of its own — a compaction summary is one — so the purpose has to be
/// first-class: a budget whose summarisation spend is indistinguishable from answer spend is a
/// budget nobody can explain after the fact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    /// The answer the caller is waiting for.
    #[default]
    Answer,
    /// The kernel's own bookkeeping.
    Summary,
    /// Another try at a turn that produced something unusable.
    Repair,
}

/// The route a call takes, in order.
///
/// A chain rather than a name because the kernel owns retries and fallback: a runtime that
/// gives up on the first 503 has the availability of its least reliable provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Routing {
    /// Candidate models, first that answers wins.
    pub chain: Vec<String>,
    /// Extra attempts against the same target before moving to the next.
    pub retries: u32,
}

/// Where in a call an attempt happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttemptPhase {
    /// Compiling and checking, before anything was sent.
    Preflight,
    /// The provider call itself.
    Execute,
    /// Refused before it happened.
    CircuitOpen,
}

/// What caused an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttemptTrigger {
    /// The first attempt for the chosen target.
    Selection,
    /// Another try at the same target.
    Retry,
    /// A hop to the next target.
    Fallback,
}

/// How an attempt ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AttemptOutcome {
    /// It answered.
    Ok,
    /// It did not. Present even when a partial answer was streamed, because that one still
    /// cost money.
    Failed {
        /// A stable category.
        code: ErrorCode,
        /// For humans.
        reason: String,
    },
}

/// A field the provider could not honour.
///
/// Recorded rather than swallowed: a `tool_choice = Required` that quietly became `Auto` is a
/// session behaving differently than it was configured to, and the difference is
/// undiscoverable afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Degraded {
    /// The request field, e.g. `tool_choice`.
    pub field: String,
    /// Why, in the provider's terms.
    pub reason: String,
}

/// One attempt, recorded whether or not it produced anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Attempt {
    /// Position in the ledger.
    pub index: u32,
    /// The target. Empty for a preflight refusal, which never had one.
    pub model: String,
    /// Where it happened.
    pub phase: AttemptPhase,
    /// What caused it.
    pub trigger: AttemptTrigger,
    /// How it ended.
    pub outcome: AttemptOutcome,
    /// What it consumed, if anything.
    pub usage: Usage,
    /// How long it took.
    pub duration_millis: u64,
    /// Fields this attempt could not honour.
    pub degraded: Vec<Degraded>,
}

/// Every attempt a call made, and the sum.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Ledger {
    /// In order.
    pub attempts: Vec<Attempt>,
    /// The sum across every attempt that produced tokens.
    pub total: Usage,
}

impl Ledger {
    /// Record an attempt and fold its usage into the total.
    ///
    /// Folding happens here rather than at the call site so that a failed attempt that still
    /// streamed cannot be quietly left out of the bill.
    ///
    /// The first attempt *defines* the accumulator instead of being added to it: a fresh total
    /// is zeros-and-unknown, and "nothing recorded yet" is not the same as "a report was
    /// missing". Adding the first report to an empty total would otherwise make every cost
    /// unknown — which is what an empty total looks like, not what the attempt said.
    pub fn record(&mut self, attempt: Attempt) {
        if self.attempts.is_empty() {
            self.total = attempt.usage.clone();
        } else {
            self.total.add(&attempt.usage);
        }
        self.attempts.push(attempt);
    }

    /// How many attempts were made.
    #[must_use]
    pub fn len(&self) -> usize {
        self.attempts.len()
    }

    /// Whether nothing was attempted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.attempts.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Generation parameters
// ---------------------------------------------------------------------------

/// How the model should choose among the tools it was given.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// It decides.
    #[default]
    Auto,
    /// It must call one.
    Required,
    /// It must not call one.
    None,
}

/// How hard a reasoning model should think.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    /// Barely.
    Minimal,
    /// A little.
    Low,
    /// Normal.
    Medium,
    /// As much as it costs.
    High,
}

/// The shape the answer must have.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TextFormat {
    /// Prose.
    #[default]
    Text,
    /// A JSON object, schema unchecked.
    JsonObject,
    /// A JSON object that must match a schema.
    JsonSchema {
        /// The name the provider files it under.
        name: String,
        /// The schema, serialized.
        schema_json: String,
    },
}

/// What the caller wants from one call.
///
/// Every field is carried, not decided: the kernel has no opinion about sampling. They live
/// here rather than as a bag of extras because a bag is a second extension hatch, and these are
/// the parameters every provider has.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GenerateParams {
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
    ///
    /// Not every provider has this — the OpenAI Responses API rejects it outright — so an adapter
    /// that cannot send it reports a [`Degraded`] rather than dropping it in silence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    /// Reasoning effort, for models that expose one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// How to choose among tools.
    #[serde(default)]
    pub tool_choice: ToolChoice,
    /// Whether one turn may call several tools at once.
    ///
    /// In the contract rather than in an adapter because the *kernel* acts on it: `invoke_tools`
    /// fans a batch out concurrently. A provider that forbids it and a kernel that does it anyway
    /// would disagree about what happened in a turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// The shape the answer must have.
    #[serde(default)]
    pub format: TextFormat,
}

// ---------------------------------------------------------------------------
// Sandboxing
// ---------------------------------------------------------------------------

/// What a session's commands may touch.
///
/// Set when the session opens and carried in its checkpoint. It is **not** a field of a request:
/// a caller that can name its own policy can name `DangerFullAccess`, and a permission slip the
/// untrusted side writes is not a permission model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SandboxPolicy {
    /// No restriction. Use with caution, and only where the code is yours.
    DangerFullAccess,
    /// Read everything, write nothing.
    ReadOnly {
        /// Whether outbound network is allowed.
        #[serde(default)]
        network_access: bool,
    },
    /// Write inside the session's workspace and nothing else.
    WorkspaceWrite {
        /// Extra roots outside the workspace that may be written.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        extra_writable_roots: Vec<String>,
        /// Whether outbound network is allowed.
        #[serde(default)]
        network_access: bool,
    },
    /// The process is already outside our reach; only the network setting is ours to honour.
    ExternalSandbox {
        /// Whether outbound network is allowed.
        #[serde(default)]
        network_access: bool,
    },
}

/// The default is the useful one, not the loosest: write inside the workspace, no network.
impl Default for SandboxPolicy {
    fn default() -> Self {
        Self::WorkspaceWrite {
            extra_writable_roots: Vec::new(),
            network_access: false,
        }
    }
}

impl SandboxPolicy {
    /// The narrowest policy: read only, no network.
    #[must_use]
    pub fn read_only() -> Self {
        Self::ReadOnly {
            network_access: false,
        }
    }

    /// Whether outbound network is permitted under this policy.
    #[must_use]
    pub fn network_access(&self) -> bool {
        match self {
            Self::DangerFullAccess => true,
            Self::ReadOnly { network_access }
            | Self::WorkspaceWrite { network_access, .. }
            | Self::ExternalSandbox { network_access } => *network_access,
        }
    }
}

/// Extra room asked for by one command.
///
/// Only ever widens: a caller cannot use it to narrow the session's policy, because narrowing by
/// request would let one command quietly change what a later command is allowed to assume.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdditionalPermissions {
    /// Roots outside the workspace this command may write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable_roots: Vec<String>,
    /// Whether this command may reach the network.
    #[serde(default)]
    pub network_access: bool,
}

/// What a single `exec` asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SandboxPermissions {
    /// Run under the session's policy.
    #[default]
    UseDefault,
    /// Run outside the sandbox. The gate turns this into a human question, not a formality.
    RequireEscalated,
    /// Stay in the sandbox, with more room for this command only.
    WithAdditionalPermissions {
        /// What to add. Never what to remove.
        additional: AdditionalPermissions,
    },
}

/// Which stream a chunk came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// A piece of captured output, and whether it was cut.
///
/// Truncation is carried rather than applied silently: a model that cannot see that its output
/// was shortened will reason about a file it only half read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OutputChunk {
    /// The text kept.
    pub text: String,
    /// The line after which the rest was dropped, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_after_lines: Option<u32>,
}

/// How one command ended.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExecOutput {
    /// The exit status, or `-1` when the process was killed by a signal.
    pub exit_code: i32,
    /// Standard output.
    pub stdout: OutputChunk,
    /// Standard error.
    pub stderr: OutputChunk,
    /// How long it ran.
    pub duration_millis: u64,
    /// Whether it was killed for exceeding its deadline.
    pub timed_out: bool,
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

/// What a session is for.
///
/// A process session is not a second kind of object: it is the same lifecycle — single writer,
/// event log, budget, sandbox, checkpoint — with a different syscall face. In Unix a shell
/// session is a process, and the same reasoning applies here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// Runs a model loop.
    #[default]
    Agent,
    /// Runs one command, and stays open while it does.
    Process,
}

/// How to start a process session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessSpec {
    /// The command line. No shell by default: an argv cannot be word-split by accident.
    pub argv: Vec<String>,
    /// Where to run it. Absent means the session's workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Environment to add.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// What this command asks for on top of the session's policy.
    #[serde(default)]
    pub permissions: SandboxPermissions,
}

/// Where a process is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProcessStatus {
    /// No pid yet: the session exists, the process does not.
    #[default]
    Pending,
    /// Running, under whatever isolation the platform could give.
    Running {
        /// The process id.
        pid: u32,
    },
    /// It ended.
    Exited {
        /// How.
        output: ExecOutput,
    },
    /// The process is gone and we do not know how it ended.
    ///
    /// A live process cannot be reconstructed from a log — its memory, its file descriptors and
    /// its cursor are not in the event stream. So a session that outlives the runtime says this,
    /// rather than pretending the command is still running.
    Lost,
}

/// A process session's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessState {
    /// How it was started.
    pub spec: ProcessSpec,
    /// Where it is now.
    pub status: ProcessStatus,
}

/// Where a process is, without the payload.
///
/// A filter asks "which ended?", not "which ended with exit code 3", so the tag is its own type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProcessPhase {
    /// No pid yet.
    Pending,
    /// Running.
    Running,
    /// It ended.
    Exited,
    /// It is gone and we do not know how it ended.
    Lost,
}

impl ProcessStatus {
    /// The phase, without the payload.
    #[must_use]
    pub fn phase(&self) -> ProcessPhase {
        match self {
            Self::Pending => ProcessPhase::Pending,
            Self::Running { .. } => ProcessPhase::Running,
            Self::Exited { .. } => ProcessPhase::Exited,
            Self::Lost => ProcessPhase::Lost,
        }
    }
}

impl ProcessState {
    /// Whether the process has ended, one way or another.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        matches!(
            self.status,
            ProcessStatus::Exited { .. } | ProcessStatus::Lost
        )
    }
}

// ---------------------------------------------------------------------------
// Hints: the parameters that belong to a session, not to a call
// ---------------------------------------------------------------------------

/// Preferences an adapter may honour, and must report when it cannot.
///
/// Separate from [`GenerateParams`] because these change while a session runs — a conversation
/// that is filling its window wants less prose, and a review turn wants more detail — and because
/// they are *hints*: an adapter that has no channel for one reports a [`Degraded`] rather than
/// failing the call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Hints {
    /// How much prose an answer should carry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<Verbosity>,
    /// How much of the reasoning to summarise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_summary: Option<Summary>,
    /// The cost/latency trade this session prefers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    /// A ceiling on tool calls in one response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    /// What to do when the conversation does not fit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<Truncation>,
    /// Labels the provider echoes back, up to sixteen.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

impl Hints {
    /// A patch that leaves every field `None` changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Take every field this patch sets, and keep the rest.
    #[must_use]
    pub fn merged(&self, over: &Self) -> Self {
        Self {
            verbosity: over.verbosity.or(self.verbosity),
            reasoning_summary: over.reasoning_summary.or(self.reasoning_summary),
            service_tier: over.service_tier.or(self.service_tier),
            max_tool_calls: over.max_tool_calls.or(self.max_tool_calls),
            truncation: over.truncation.or(self.truncation),
            metadata: if over.metadata.is_empty() {
                self.metadata.clone()
            } else {
                over.metadata.clone()
            },
        }
    }
}

/// How much prose an answer should carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verbosity {
    /// Terse.
    Low,
    /// Normal.
    Medium,
    /// Expansive.
    High,
}

/// How much of the reasoning to summarise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Summary {
    /// The provider decides.
    Auto,
    /// Short.
    Concise,
    /// Long.
    Detailed,
}

/// The cost/latency trade a session prefers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    /// Let the provider decide.
    Auto,
    /// The default tier.
    Default,
    /// Cheaper, slower.
    Flex,
    /// More expensive, faster.
    Priority,
}

/// What to do when the conversation does not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Truncation {
    /// Drop the oldest items.
    Auto,
    /// Fail instead of dropping silently.
    Disabled,
}

/// A partial update to a session's mutable configuration.
///
/// The wiring — kind, sandbox, workspace — is not here, because it is decided when the session
/// opens and is not the application's to change while it runs. Everything in this struct is
/// content or strategy, and every field is optional: absent means unchanged, `null` (where the
/// field is doubly optional) means cleared.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigPatch {
    /// Standing instructions. `Some(None)` clears them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<Option<String>>,
    /// Generation parameters. Replaces the whole block, because every field is already optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<GenerateParams>,
    /// Adapter hints. Merged field by field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hints: Option<Hints>,
    /// The tool allow-list. Empty means every registered tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// The route this session takes. Absent means unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<Routing>,
}

// ---------------------------------------------------------------------------
// Listing processes
// ---------------------------------------------------------------------------

/// What a listing asks for.
///
/// A keyset cursor rather than an offset: sessions are created and retired constantly, and an
/// offset would skip or repeat rows exactly when a process ends — which is the moment a caller is
/// most likely to be paging.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessQuery {
    /// Only sessions in this phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<ProcessPhase>,
    /// How many to return. Zero asks for the default.
    #[serde(default)]
    pub limit: u32,
    /// Continue after this session id. Absent starts at the beginning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<SessionId>,
}

impl ProcessQuery {
    /// The page size this query asks for.
    #[must_use]
    pub fn page_size(&self) -> usize {
        match self.limit {
            0 => 50,
            limit => usize::try_from(limit.min(500)).unwrap_or(50),
        }
    }
}

/// One page of a listing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessPage {
    /// The sessions in this page, in id order.
    pub processes: Vec<(SessionId, ProcessState)>,
    /// Where to continue. Absent means this was the last page.
    pub next: Option<SessionId>,
}
