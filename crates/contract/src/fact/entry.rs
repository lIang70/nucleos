use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Authority;
use crate::content::ToolName;
use crate::error::ErrorKind;
use crate::inference::Outcome;
use crate::inference::{FinishReason, ModelRef};

use super::{BlobId, Seq};
use crate::SessionKey;

/// One entry of a session's log.
///
/// The log is the state, not a trace of it: a session's current authority and its running call are
/// derived by reading entries in [`Seq`] order, so a restart never has to reconcile a record with a
/// separate mutable row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Fact {
    /// Position in this session's log.
    pub seq: Seq,
    /// Which session wrote it.
    pub session: SessionKey,
    /// Unix milliseconds, read from the injected clock.
    ///
    /// Not the store's clock: a test needs a timestamp it chose, and a fact must be identical
    /// however many times it is replayed.
    pub at_unix_millis: u64,
    /// The trace this entry belongs to, when it belongs to one.
    ///
    /// A typed field rather than an attribute map: a reader may rely on it, and an open map is
    /// where a field goes when nobody has decided what it means.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// What happened.
    pub kind: FactKind,
}

impl Fact {
    /// Build one entry.
    #[must_use]
    pub fn new(seq: Seq, session: SessionKey, at_unix_millis: u64, kind: FactKind) -> Self {
        Self {
            seq,
            session,
            at_unix_millis,
            trace_id: None,
            kind,
        }
    }

    /// Attach the trace this entry belongs to.
    #[must_use]
    pub fn traced(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }
}

/// Why a session was released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    /// The caller asked for it.
    Caller,
    /// A failure left it in a state it could not continue from.
    Error,
}

/// Where one side effect stands.
///
/// The durable half of "did this run". A `Started` with no matching `Finished` or `Failed` is the
/// only honest record of a crash mid-effect, and it is why a resume refuses to guess: one guess
/// repeats the effect, the other skips it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EffectStatus {
    /// Written before the effect runs. A record here with no ending means the outcome is unknown.
    Started,
    /// Written after it returned.
    Finished,
    /// Written after it failed.
    Failed,
}

/// What happened.
///
/// One variant per state change a reader must be able to see, with no catch-all: a change that
/// cannot be named here cannot be recorded, which is what keeps "unclassified" from becoming a
/// silent state a reader is allowed to ignore.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FactKind {
    /// The session came into existence with this plan.
    SessionOpened {
        /// The plan the runtime checked against the application envelope.
        authority: Authority,
        /// The envelope version that accepted it. Audit only: enforcement reads the current
        /// envelope, so a later version restricts this session without another fact.
        envelope_version: u64,
    },
    /// Its plan was replaced.
    SessionConfigured {
        /// The new plan, already checked against the current envelope.
        authority: Authority,
        /// The envelope version it was checked against.
        envelope_version: u64,
    },
    /// The session was released. Reopening is a new `SessionOpened` at a later position.
    SessionClosed {
        /// Why it stopped, so a reader does not have to infer it from the gap that follows.
        reason: CloseReason,
    },
    /// A model call began. This is the commit point: before it a refusal costs nothing, after it the
    /// log is the record and the caller learns the outcome from entries rather than from a
    /// connection it happened to keep open.
    ///
    /// Named `ModelCall` rather than `Turn` because a turn is a conversational unit — one question
    /// and one answer — while this is one invocation, which may be the second of several after a
    /// fallback.
    ModelCallStarted {
        /// Which model was asked.
        model: ModelRef,
    },
    /// The conversation that was actually sent, addressed rather than repeated.
    PromptSent {
        /// The conversation bytes.
        conversation: BlobId,
        /// How many messages it contained.
        messages: u32,
        /// How many bytes it was, so a reader can see the cost without loading it.
        bytes: u64,
    },
    /// The model call produced an assistant message.
    ModelCallCompleted {
        /// Which model actually answered, which under fallback is not the one that was asked.
        model: ModelRef,
        /// The message, addressed.
        message: BlobId,
        /// Why generation stopped.
        finish: FinishReason,
        /// What answered, and what it consumed. One value for both halves, because a record that
        /// kept them apart is a record a reader has to join.
        outcome: Outcome,
    },
    /// The model call failed after its commit point.
    ///
    /// `outcome` is present when anything was reported and absent when nothing was: a stream that
    /// broke halfway may already have cost money, and claiming zero would erase that from the log.
    ModelCallFailed {
        /// What kind of failure.
        error: ErrorKind,
        /// For a human.
        message: String,
        /// What was reported before the failure, when anything was.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<Outcome>,
    },
    /// An embedding batch completed.
    ///
    /// Recorded because it spends: a log that named only generations cannot reconstruct a bill.
    EmbeddingCompleted {
        /// Which embedding model answered.
        model: ModelRef,
        /// How many documents were embedded.
        vectors: u32,
        /// What answered, and what it consumed.
        outcome: Outcome,
    },
    /// A decision batch completed, for the same reason.
    DecisionCompleted {
        /// Which model answered.
        model: ModelRef,
        /// How many questions were answered.
        answers: u32,
        /// What answered, and what it consumed.
        outcome: Outcome,
    },
    /// A tool call reached a new effect state.
    ///
    /// Written before dispatch as `Started`, so a crash leaves a call whose outcome nobody recorded
    /// instead of a call that silently looks unstarted.
    ToolCallRecorded {
        /// The call id the model issued.
        call_id: String,
        /// Which tool.
        tool: ToolName,
        /// Where the effect stands.
        status: EffectStatus,
        /// The result, once there is one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<BlobId>,
    },
    /// A caller stated an approval for a call.
    ///
    /// Recorded because the kernel never sees a human: this fact is exactly as much as parking a
    /// session ever proved, and it is the difference between "the model asked" and "the caller said
    /// a user agreed".
    ToolCallApproved {
        /// Which call.
        call_id: String,
        /// Which offered option the caller stated.
        option_id: String,
    },
}
