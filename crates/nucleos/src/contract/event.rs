//! The log: one append-only record of everything a session did.
//!
//! An event is the only way state changes. Nothing here is a message: a
//! [`PartAppend`] is a fragment of one, and the message exists only once
//! [`MessageCompleted`] says it finished.

use super::{
    Attempt, ConfigPatch, Degraded, ErrorCode, ExecOutput, ExecStream, Generation, Message, Part,
    PermissionKind, PermissionOption, Purpose, SandboxPolicy, SessionId, SessionState, StopReason,
    ToolResult, Usage,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    ///
    /// Flattened, like [`Part::kind`]: without it the wire says `{"kind":{"kind":"text_delta",…}}`,
    /// which is the tag twice and a shape nobody writes by hand. It also makes the SSE event name and
    /// the JSON tag the same field at the same level, so a client that reads only `data` and one that
    /// reads only the event name cannot end up with different ideas of what the frame is.
    #[serde(flatten)]
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
        /// Set when the state is terminal. Omitted otherwise: `null` is not used on this wire.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop: Option<StopReason>,
    },
    /// A fragment of assistant text.
    /// A fragment appended to the part at `part_index`.
    ///
    /// The merge rule is a property of the *event*, never inferred from the payload: text and
    /// reasoning arrive as fragments and are appended, and a media part that streams arrives as a
    /// sequence of chunks whose bytes are appended. A part that arrives whole uses
    /// [`EventKind::PartReplace`] instead.
    ///
    /// The distinction is not cosmetic. Synthesized audio is a sequence of chunks at one index;
    /// under a single "delta" event whose rule was inferred from the payload, every chunk would have
    /// replaced the last and only the final fragment would have survived.
    PartAppend {
        /// Which part of the message it belongs to.
        ///
        /// Without it a client cannot tell a second text block from a continuation of the first, so
        /// text → tool call → more text is unrepresentable in the stream.
        part_index: u32,
        /// The fragment.
        part: Part,
    },
    /// A whole part at `part_index`, replacing whatever was there.
    ///
    /// What a generated image or a file reference arrives as, and what a preview becomes when the
    /// final version is ready.
    PartReplace {
        /// Which part of the message it belongs to.
        part_index: u32,
        /// The part.
        part: Part,
    },
    /// A fragment of a reasoning trace.
    ///
    /// `signature` usually arrives on its own, in a delta with empty `text`: a provider
    /// signs the finished block rather than each fragment.

    /// A tool call began. Its arguments follow as [`EventKind::ToolCallArgs`].
    ToolCallStarted {
        /// Position in this turn's tool-call list.
        ///
        /// The tool-call ordinal, which is what [`EventKind::ToolCallArgs`] and the result are
        /// correlated by. It is not the position of the call in the message.
        index: u32,
        /// Which part of the assistant message this call occupies.
        ///
        /// The same index space as [`EventKind::PartAppend`] and [`EventKind::PartReplace`]. A tool
        /// call is a part at its own index, not a separate sequence: without it the assembled message
        /// and the event stream describe the same turn in two different orders.
        part_index: u32,
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
    /// Tools became visible to the model.
    ///
    /// Recorded because a replay has to know which tools the model could see at turn 42 — the same
    /// reason `session_configured` exists, and the same failure if it did not.
    ToolsExposed {
        /// The names that were added.
        names: Vec<String>,
    },
    /// The session's mutable configuration changed.
    ///
    /// Recorded because a replay has to know which system prompt was in force at turn 42; without
    /// it, the log would rebuild a conversation that never happened.
    SessionConfigured {
        /// What changed, field by field.
        ///
        /// Boxed because the event union must stay small enough to move around cheaply: a config
        /// patch carries strings, and one large variant would make every event that large.
        patch: Box<ConfigPatch>,
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
        /// Which parked question this is. A tool call's id, or the session's for a command.
        pending_id: String,
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
    /// Credit was granted to a session.
    ///
    /// An event rather than a silent mutation: a replay that did not see this would think the session
    /// ran out of money when it did not, and a session that was revived would look like one that never
    /// stopped.
    Credited {
        /// How much was granted. `None` means the limit was removed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        amount: Option<u64>,
        /// The balance after the grant. `None` means unlimited.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remaining: Option<u64>,
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
    /// A turn began, and everything after this event belongs to it.
    ///
    /// Written before the first model call, and only once the turn is certain to run: budget spent,
    /// route resolved, policy satisfied. That makes it the line a follower watches — before it, a
    /// failure is the caller's to handle and belongs in a status code; after it, the log is the record
    /// of what happened, and a stream that stopped silently is distinguishable from one that stalled.
    TurnStarted {
        /// The model the route resolved to first.
        model: String,
        /// The generation this turn runs under.
        generation: u64,
        /// Which step of the session this is, 1-based.
        ///
        /// Carried because the step counter is a budget: a turn that fails after it began still spent
        /// one, and a reload that could not see it would hand the step back — making `max_steps`
        /// enforceable per process rather than per session.
        #[serde(default)]
        step: u64,
    },
    /// The turn ended without producing a message and without a failed attempt to point at.
    ///
    /// Not every failure is an attempt's: a policy that refuses the model model-level, a prompt that
    /// cannot fit, a store that refused the write on the way in. Without this event those turns leave
    /// the log with a `turn_started` and nothing else — a state indistinguishable from a turn that is
    /// still running, which is precisely what a caller cannot act on.
    TurnFailed {
        /// Why, in the kernel's own vocabulary.
        code: crate::contract::ErrorCode,
        /// What it said.
        reason: String,
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
        /// The provider's response identifier, when the wire carries one (OpenAI `response.id`).
        ///
        /// Empty when the provider exposes none. It is how a caller correlates an answer with the
        /// provider's own logs without inspecting the request or the payload.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
        /// The provider's request identifier, when the wire carries one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    /// A command started.
    ExecStarted {
        /// The process id, when the platform gave one.
        pid: u32,
        /// What was run, verbatim, so an audit can see it.
        argv: Vec<String>,
        /// The policy it ran under, after any granted widening.
        policy: SandboxPolicy,
        /// The mechanism that carried the policy out, when one did.
        ///
        /// Absent means the kernel applied nothing: `danger_full_access`, an external sandbox, or an
        /// approved escalation. Recorded because "was it confined" is the first question asked after
        /// a command does something surprising, and the policy alone does not answer it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mechanism: Option<String>,
        /// The profile the mechanism was given, verbatim.
        ///
        /// A refusal from a sandbox is "Operation not permitted" and nothing else. Without this the
        /// log cannot say which rule was missing, and the only way to answer is to rebuild the
        /// profile from a policy that may since have changed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
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
    /// An event this build does not understand.
    ///
    /// Written by a newer daemon and read by an older one: carried through loading rather than
    /// refused, because a store is shared by whichever builds happen to run against it and "cannot
    /// parse one row" is not a state a durable log may have. `wire.md` says the same thing about the
    /// wire — an unknown union member is carried, not failed — and this is the half that applies to
    /// the record.
    ///
    /// The original kind name is **not** preserved: serde's `other` arm keeps the event, not the tag.
    /// That is deliberate. Rewriting an event is something this kernel never does, so the only copy of
    /// the original body is the stored one, and the correct behaviour for a build that does not
    /// understand an event is to leave it exactly as it found it.
    ///
    /// **What this costs, stated where a client author will read it.** The sentinel is what a stream
    /// delivers: an event whose kind this build does not know arrives as `type_name()`
    /// `nucleos.v1.unknown` with `kind: "unknown"` and **no payload**, because the frame is built by
    /// re-serialising the decoded value and the decoding kept only the tag's failure. The store is the
    /// half that keeps the bytes; the stream is the half that loses them. So a client must treat
    /// `unknown` as "a frame I cannot interpret" rather than as an error, and must not expect the
    /// payload to survive a hop through an older daemon.
    #[serde(other)]
    Unknown,
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
            Self::PartAppend { .. } => "nucleos.v1.part_append",
            Self::PartReplace { .. } => "nucleos.v1.part_replace",
            Self::ToolCallStarted { .. } => "nucleos.v1.tool_call_started",
            Self::ToolFinished { .. } => "nucleos.v1.tool_finished",
            Self::ToolCallArgs { .. } => "nucleos.v1.tool_call_args",
            Self::Usage { .. } => "nucleos.v1.usage",
            Self::ExecStarted { .. } => "nucleos.v1.exec_started",
            Self::ExecOutput { .. } => "nucleos.v1.exec_output",
            Self::ExecStdin { .. } => "nucleos.v1.exec_stdin",
            Self::ExecExited { .. } => "nucleos.v1.exec_exited",
            Self::ToolsExposed { .. } => "nucleos.v1.tools_exposed",
            Self::SessionConfigured { .. } => "nucleos.v1.session_configured",
            Self::CapabilityDegraded(_) => "nucleos.v1.capability_degraded",
            Self::AttemptFailed { .. } => "nucleos.v1.attempt_failed",
            Self::AwaitingHuman { .. } => "nucleos.v1.awaiting_human",
            Self::BudgetExhausted { .. } => "nucleos.v1.budget_exhausted",
            Self::Credited { .. } => "nucleos.v1.credited",
            Self::Failed { .. } => "nucleos.v1.failed",
            Self::GenerationMigrated { .. } => "nucleos.v1.generation_migrated",
            Self::TurnStarted { .. } => "nucleos.v1.turn_started",
            Self::TurnFailed { .. } => "nucleos.v1.turn_failed",
            Self::Unknown => "nucleos.v1.unknown",
            Self::MessageCompleted { .. } => "nucleos.v1.message_completed",
            Self::Progress { .. } => "nucleos.v1.progress",
        }
    }
}
