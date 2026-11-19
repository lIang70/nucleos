//! Who is asking, what may be asked for, and what comes back.
//!
//! The kernel authenticates nobody: a [`Principal`] is data it passes to policy.
//! What is here is the vocabulary a policy answer is expressed in, and the declarations
//! an adapter makes about what it can serve.

use super::{
    Budget, GenerateParams, Generation, Hints, Ledger, Message, ProcessSpec, ProcessState, Routing,
    SandboxPolicy, SessionId, SessionKind, SessionState, StopReason, ToolCall, ToolPolicy, Usage,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    /// The request is valid but the thing it refers to moved.
    ///
    /// Distinct from `Invalid`, which blames the request. A caller whose configuration document was
    /// written against revision 3 and is being applied to revision 5 sent something correct; the
    /// answer is "re-read and try again", which is a different instruction.
    Conflict,
    /// The caller cancelled it.
    ///
    /// Distinct from `Timeout` because nothing went wrong: an operator or an application decided to
    /// stop, and the difference matters when reading a log the next morning.
    Cancelled,
    /// A dependency was not reachable.
    Unavailable,
    /// The request asks for something this model does not do: an output kind it does not produce, a
    /// content kind it does not accept, or a tool it does not call.
    ///
    /// Distinct from `Invalid`, which is a malformed request: the request is well-formed and the model
    /// is simply not the one to serve it, so the instruction is to route elsewhere rather than to fix
    /// the request.
    UnsupportedFeature,
    /// The route names a model this kernel does not have.
    UnknownModel,
    /// The adapter neither honoured nor explained a field the caller set, or claims a field the
    /// request never set.
    ///
    /// An adapter bug rather than a caller's mistake, and distinct from `Internal` because the caller
    /// can do something about it: report the adapter, not the request.
    AdapterContractViolation,
    /// The answer stopped arriving before the provider said it was finished.
    ///
    /// Its own code because the alternative is a lie: the turn would be recorded as a complete answer
    /// with a `Stop` nobody received, and the reader has no way to tell a short answer from a cut
    /// connection. Whatever arrived first stays in the log — the money was spent, and a client has
    /// already rendered it — but the turn failed.
    ProviderTruncated,
    /// The provider's payload could not be decoded into the contract's vocabulary.
    ///
    /// Distinct from `Unavailable`: this is not a dependency being down but an answer that arrived
    /// and could not be read, so retrying the same request re-sends it into the same decoder.
    InvalidProviderResponse,
    /// The call completed and its payload cannot be used: an answer with no content at all.
    ///
    /// A successful empty turn is worse than a refused one: the caller sees a turn that cost money
    /// and produced nothing, and cannot tell it from a model that had nothing to say.
    ProviderResponseUnusable,
    /// A bug.
    Internal,
}

impl ErrorCode {
    /// Whether retrying the same request could succeed.
    ///
    /// A statement about the request, not about the world: `Budget` is retryable only
    /// after the caller grants more credit.
    ///
    /// `ProviderTruncated` and `ProviderResponseUnusable` are here because the *request* is fine and
    /// a second attempt could produce a complete answer. Whether the turn is actually retried is a
    /// different question, and a stricter one: an attempt that produced content is never retried, so
    /// a stream that was cut after streaming text fails the turn rather than becoming two partial
    /// answers.
    ///
    /// `InvalidProviderResponse` is not retryable: the payload is already unreadable, and a retry
    /// hands the same wire format to the same decoder.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::Unavailable
                | Self::Timeout
                | Self::ProviderTruncated
                | Self::ProviderResponseUnusable
        )
    }
}

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

impl Default for Principal {
    /// Nobody. Used as the "unknown owner" value for a snapshot written before the field existed,
    /// never as an authenticated identity — a caller with an empty tenant never matches a real one.
    fn default() -> Self {
        Self {
            tenant: String::new(),
            subject: String::new(),
        }
    }
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
    /// How the tool is reached, derived by whoever connected it.
    ///
    /// Part of the generation's compatibility check: a tool that keeps its name and schema but now
    /// runs in-process where it used to run in a sandboxed child is a silent downgrade every other
    /// check passes. `None` means the caller did not say, which is not the same as "any" — a
    /// generation that knows must not migrate onto one that does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<Isolation>,
}

/// How a tool is reached, as a fact about the connection rather than a claim about the tool.
///
/// Derived by the daemon from how it reached the source — a spawned child, an HTTP endpoint, an
/// in-process closure — because that is a fact the daemon owns. A source asserting its own isolation
/// would be an unchecked claim dressed as a verified one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// A spawned child process, which the daemon supervises.
    Process,
    /// A remote HTTP endpoint. The process boundary is somebody else's, and so is the isolation.
    Http,
    /// In the kernel's own process: a built-in, or a test double.
    InProcess,
}

/// What a model can do.
///
/// Checked at `Start`, never discovered by trial: a model that silently ignores tools, or
/// silently truncates a prompt, changes the answer without saying so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
/// What a model can do, declared once and enforced before every call.
///
/// The rule for what belongs here, and what does not:
///
/// - **An absence that makes the call pointless is a field here**, and the kernel refuses the request
///   before any I/O: an output modality it does not produce, an input kind it does not accept, a
///   force it will not honour, a schema it cannot be held to.
/// - **An absence that only lowers quality is adapter settings, reported as [`Degraded`]**: a
///   reasoning effort with no channel, a hint the endpoint does not know. The call proceeds and the
///   degradation is recorded, because a call that is slightly worse is still the call that was asked
///   for.
///
/// A field here with no reader is worse than no field: it reads as a guarantee the caller can rely on
/// while the kernel never checks it.
pub struct Capability {
    /// Which content kinds it accepts as input.
    ///
    /// **Empty means undeclared**, which filters nothing: an adapter that has not been taught to
    /// declare must keep working, and the compiler remains the final arbiter for everything a
    /// declaration does not cover. A declared list rejects a request carrying an undeclared kind
    /// before any I/O, which is the difference between a clear refusal and a provider's 400.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<PartKindName>,
    /// Which content kinds it can produce.
    ///
    /// Same rule: empty is undeclared. A declared list is what makes `Output` checkable before a call
    /// is sent — a model that produces text and images, asked for audio, is a mistake the kernel can
    /// see without paying for it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<PartKindName>,
    /// Whether it accepts tool definitions and emits tool calls.
    pub tool_calling: bool,
    /// Whether it can be *made* to call one. A provider that accepts tools but ignores
    /// `tool_choice = required` turns a configured guarantee into a suggestion, which is why the
    /// kernel refuses the request rather than sending it.
    pub tool_choice_required: bool,
    /// Whether the answer can be held to a JSON Schema.
    ///
    /// A model that cannot be is a model whose `format: json_schema` request buys either a refusal or
    /// an unvalidated answer — the call has no purpose, so it is not sent.
    pub structured_output: bool,
    /// Total context, prompt plus completion.
    pub context_window: u32,
    /// The largest completion the provider will produce.
    pub max_output_tokens: u32,
    /// The largest inline media payload this model accepts, in bytes.
    ///
    /// `None` means undeclared, which filters nothing. A token limit says nothing about a file, so an
    /// audio clip needs its own ceiling; without one a request can be small in tokens and enormous in
    /// bytes, and the provider's refusal is discovered after the upload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_bytes: Option<u64>,
}

impl Capability {
    /// Whether this model declared nothing about the content kinds it accepts.
    #[must_use]
    pub fn accepts_undeclared_inputs(&self) -> bool {
        self.inputs.is_empty()
    }

    /// Whether this model declared nothing about the content kinds it produces.
    #[must_use]
    pub fn produces_undeclared_outputs(&self) -> bool {
        self.outputs.is_empty()
    }

    /// Check a declaration that is about to become a model a route can name.
    ///
    /// Run when a model is registered, not when a call is made. A declaration that cannot serve any
    /// call is a deployment error, and per-call discovery means discovering it in front of a user:
    /// a model with no declared window is refused by the fit check every time, and an `outputs` list
    /// naming something that is not an output is a `Output` no caller can ask for.
    ///
    /// # Errors
    ///
    /// An output kind that is not a modality, a duplicate kind, or a window of zero.
    pub fn validate(&self) -> std::result::Result<(), String> {
        let mut seen = std::collections::BTreeSet::new();
        for kind in &self.inputs {
            if !seen.insert(*kind) {
                return Err(format!("input content kind {kind:?} is declared twice"));
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for kind in &self.outputs {
            if !matches!(
                kind,
                PartKindName::Text
                    | PartKindName::Image
                    | PartKindName::Audio
                    | PartKindName::Video
            ) {
                return Err(format!(
                    "{kind:?} is not an output modality: a call can ask for text, an image, audio or \
                     video, and nothing else"
                ));
            }
            if !seen.insert(*kind) {
                return Err(format!("output content kind {kind:?} is declared twice"));
            }
        }
        if self.context_window == 0 {
            return Err(
                "a model that declares no context window cannot be held to one, so every call to it \
                 would be refused: declare the window"
                    .to_owned(),
            );
        }
        Ok(())
    }
}

/// What a call asks the model to produce.
///
/// The only modality switch in the contract: the same request type, the same response type and the
/// same `invoke` serve every output, because the answer is a [`Content`] whose parts say what
/// arrived. A modality that lands later adds one constant, not a request type — and everything
/// modality-specific that is not the modality (an image's size, a voice) is adapter configuration or
/// an extension, never a field here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Output {
    /// The next assistant turn: text, tool calls, or both.
    #[default]
    Text,
    /// Generated images.
    Image,
    /// Synthesized speech.
    Audio,
    /// Generated video.
    Video,
}

impl Output {
    /// The content kind this output produces, for a capability check.
    #[must_use]
    pub fn part_kind(self) -> PartKindName {
        match self {
            Self::Text => PartKindName::Text,
            Self::Image => PartKindName::Image,
            Self::Audio => PartKindName::Audio,
            Self::Video => PartKindName::Video,
        }
    }
}

/// The content kinds, without their payloads.
///
/// [`PartKind`] carries data, which makes it unusable as a set member for a capability check; this is
/// the same vocabulary with nothing in it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PartKindName {
    /// Text.
    Text,
    /// An image.
    Image,
    /// A file reference.
    File,
    /// Structured data.
    Data,
    /// A tool call.
    ToolCall,
    /// A tool result.
    ToolResult,
    /// A reasoning trace.
    Reasoning,
    /// Synthesized audio.
    Audio,
    /// Generated video.
    Video,
}

/// How strongly a tool's execution is separated from the kernel.
///
/// A call waiting on a human.
///
/// Part of the snapshot because the wait outlives any process: a session parked on a
/// confirmation must be resumable tomorrow, and the call it is holding has to come back with
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PendingCall {
    /// What this question is about, and how it is identified in a log.
    pub id: String,
    /// What has not run yet.
    pub action: PendingAction,
    /// What may be answered.
    pub options: Vec<PermissionOption>,
    /// What was asked.
    pub question: String,
}

/// What a parked question is about.
///
/// Approval is not a tool concept: a policy can ask a human about a tool call *or* about running a
/// command outside its sandbox, and both are answered the same way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingAction {
    /// A tool call the policy asked about.
    Tool {
        /// The call.
        call: ToolCall,
    },
    /// A command the policy asked about — an escalation, or a program nobody has allowed yet.
    Exec {
        /// What would run, exactly.
        spec: ProcessSpec,
    },
}

/// A session's state, materialised at a point in its log.
///
/// `seq` is the event this snapshot already includes, so recovery is `load(snapshot)` then
/// `events_since(snapshot.seq + 1)`. Two numbers that must agree, written down once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Snapshot {
    /// The session it belongs to.
    pub session: SessionId,
    /// Who owns it.
    ///
    /// Recorded because the store keys sessions by id alone: without this, a second tenant could
    /// load the first tenant's session by guessing its name. `load` refuses a mismatch, and an
    /// identifier that already belongs to somebody else cannot be opened.
    #[serde(default)]
    pub principal: Principal,
    /// The last event folded in.
    pub seq: u64,
    /// How many times this session has been checkpointed.
    ///
    /// The store refuses a write that is not strictly newer, which is what makes a **stale handle**
    /// detectable: two requests can load the same session, and without this the second one to finish
    /// overwrites the first one's work with a materialisation of a log it read before that work
    /// existed. The version is the only thing in the snapshot that says "I have seen less than you".
    #[serde(default)]
    pub version: u64,
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
    /// The limits this session was opened with.
    ///
    /// In the checkpoint for the same reason the credit is: over the ABI every request loads the
    /// session, and a budget that is re-supplied per request is a budget the caller can forget — or
    /// quietly widen — by sending a different one. What a session was opened with is part of what the
    /// session *is*.
    #[serde(default)]
    pub budget: Budget,
    /// Wall-clock milliseconds the session has already burned.
    ///
    /// Separate from the start time because the two answer different questions. A session that waited
    /// an hour for a human verdict has not spent an hour of model time, and a `max_wall_millis` that
    /// charged it would fire on every session that used parked calls. Accumulated at each checkpoint,
    /// so a reload continues the clock instead of restarting it — which is what made the wall budget
    /// impossible to hit over the ABI.
    #[serde(default)]
    pub spent_wall_millis: u64,
    /// Every attempt this session has made, and the bill they add up to.
    ///
    /// In the checkpoint because it is the record, not a statistic: a resumed session that reported
    /// nothing spent would under-report exactly the sessions that were interrupted, which are the ones
    /// somebody is reconciling.
    #[serde(default)]
    pub ledger: Ledger,
    /// Tokens granted and not yet spent. `None` means no limit.
    ///
    /// In the checkpoint because every syscall may be served by a different process: a budget
    /// that resets on reload is not a budget.
    pub credit_tokens: Option<u64>,
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
    /// The exposure policy in force.
    #[serde(default)]
    pub tool_policy: ToolPolicy,
    /// Tools discovered or used, and the turn each was last reached.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub discovered: BTreeMap<String, u64>,
    /// Turns taken, which the pool expires against.
    #[serde(default)]
    pub round: u64,
    /// Why the session stopped, once it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopReason>,
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
