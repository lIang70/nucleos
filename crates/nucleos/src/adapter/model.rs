//! The model, and the shapes a provider adapter has to speak.
//!
//! The kernel knows one request shape and one delta shape. A provider with more concepts than
//! these lowers them on the way in and reports what it could not carry on the way out — the
//! alternative is a union of every provider's vocabulary, which is the thing this kernel
//! exists not to be.

use crate::contract::{Capability, Message, Purpose, ToolSpec, Usage};
use crate::error::Result;
use async_trait::async_trait;
use std::pin::Pin;
use tokio_stream::Stream;

/// A stream of deltas.
///
/// `Send` so a driver can hold a session across tasks; `'static` so it can outlive the
/// call that produced it.
pub type ModelStream = Pin<Box<dyn Stream<Item = Result<ModelDelta>> + Send>>;

/// One model, behind the only interface the kernel has.
#[async_trait]
pub trait Model: Send + Sync {
    /// What this model can do. Read before the call, never discovered by trial.
    fn capability(&self) -> Capability;

    /// How many prompt tokens this request will consume.
    ///
    /// Required, with no default. A context budget the kernel cannot compute is a budget the
    /// kernel cannot enforce, and a default that estimates by character count is a guess made
    /// precisely where accuracy matters. An adapter without a tokenizer returns a conservative
    /// estimate and says so; the kernel does not decide that for it.
    fn count_tokens(&self, request: &ModelRequest) -> Result<u64>;

    /// Which provider this adapter speaks for.
    ///
    /// Extensions addressed to another provider are inert rather than errors; the default says
    /// nothing, so every extension is inert to it.
    fn provider_id(&self) -> &'static str {
        "unknown"
    }

    /// Turn the request into the one thing this call will send, and say what that honoured.
    ///
    /// **One compilation, once.** The kernel compares the returned [`Plan`] against
    /// [`ModelRequest::active_fields`] and fails the attempt when a field was neither honoured nor
    /// explained — and because [`Prepared`] carries the compiled artifact onward to
    /// [`Model::invoke`], the thing that was checked and the thing that is sent cannot differ. An
    /// adapter that compiled twice, once to answer this and once to send, would be able to pass the
    /// check with a request it does not send.
    ///
    /// The default stashes the request itself, which is the honest answer for an adapter with no
    /// compiled form: nothing was dropped, because nothing was compiled.
    fn prepare(&self, request: ModelRequest) -> Result<Prepared> {
        let plan = Plan::assume_all(&request);
        Ok(Prepared::new(plan, request))
    }

    /// Send what [`Model::prepare`] produced.
    ///
    /// An error here means the call never started — a bad request, a refused credential, an
    /// unreachable provider. An error *inside* the stream means it started and then failed,
    /// which is the case that still owes money and still has to be recorded.
    ///
    /// The canonical request is deliberately not passed: an adapter that wants it has it in the
    /// artifact, and an adapter that wants to re-derive what to send cannot.
    async fn invoke(&self, prepared: Prepared) -> Result<ModelStream>;
}

/// One request, compiled by the adapter that is about to send it.
///
/// The two halves are the whole of the contract check: [`Plan`] says what was accounted for, the
/// artifact is what is actually sent. Keeping them in one value is what makes "the plan is a
/// promise" enforceable — see [`Model::prepare`].
#[derive(Debug)]
pub struct Prepared {
    plan: Plan,
    artifact: Box<dyn std::any::Any + Send>,
}

impl Prepared {
    /// Bundle a plan with the artifact it describes.
    #[must_use]
    pub fn new<T: std::any::Any + Send>(plan: Plan, artifact: T) -> Self {
        Self {
            plan,
            artifact: Box::new(artifact),
        }
    }

    /// What this call will honour, and what it will honour only in part.
    #[must_use]
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// The compiled artifact, if it is of type `T`. `None` means the adapter compiled something
    /// else, or nothing at all.
    #[must_use]
    pub fn artifact<T: std::any::Any>(&self) -> Option<&T> {
        self.artifact.downcast_ref::<T>()
    }

    /// Take the compiled artifact, consuming the value. An adapter needs this to move a body into
    /// a `'static` stream rather than borrowing it.
    #[must_use]
    pub fn take_artifact<T: std::any::Any + Send>(self) -> Option<T> {
        self.artifact.downcast::<T>().ok().map(|boxed| *boxed)
    }

    /// The request this was compiled from, when the adapter had no compiled form of its own.
    ///
    /// This is the default [`Model::prepare`]'s artifact, and nothing else: an adapter that
    /// overrides `prepare` returns its own type, and this returns `None` — the canonical request
    /// is not how a compiled call describes itself.
    #[must_use]
    pub fn request(&self) -> Option<&ModelRequest> {
        self.artifact::<ModelRequest>()
    }
}

/// Everything the kernel sends.
///
/// `PartialEq` but not `Eq`: sampling parameters are floats, and a float has no total order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelRequest {
    /// Standing instructions, if the session has any.
    pub system: Option<String>,
    /// The conversation, oldest first.
    ///
    /// Shared, not copied: a turn builds one request per attempt, and a long session would otherwise
    /// clone its whole history for each one. The adapter only reads it.
    pub messages: std::sync::Arc<[Message]>,
    /// The tools this turn may use.
    pub tools: Vec<ToolSpec>,
    /// How to shape the answer.
    pub params: GenerateParams,
    /// The session's hints, for an adapter that has a channel for them.
    pub hints: crate::contract::Hints,
    /// What this call asks the model to produce.
    pub output: crate::contract::Output,
    /// Why this call is happening.
    pub purpose: Purpose,
    /// Provider-specific options. Each adapter applies only its own and treats the rest as inert.
    pub extensions: crate::adapter::ExtensionSet,
    /// Cross-cutting correlation data, filled by the kernel and never interpreted by it.
    ///
    /// This is not a request knob: it does not change what the caller asked for. It is how an
    /// adapter derives provider-side correlation (a cache key shared by a session's turns, a privacy
    /// preserving end-user identifier) without making those values session configuration.
    pub correlation: Option<std::sync::Arc<Correlation>>,
}

/// The facts about the call that every layer may read and no layer owns.
///
/// A typed value rather than a stringly map: the kernel already knows these four things, and a map
/// would let an adapter misspell one and silently lose its cache affinity. `turn` identifies the
/// current attempt's turn within the session; it is deliberately *not* part of a cache key, which
/// should be shared by the session's turns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Correlation {
    /// The session this call belongs to.
    pub session: String,
    /// The current turn number.
    pub turn: u64,
    /// The tenant the caller belongs to.
    pub tenant: String,
    /// The subject within that tenant.
    ///
    /// Included because an end-user identifier is about the subject, not about the session: two
    /// sessions belonging to one caller should not look like two people to the provider.
    pub subject: String,
}

impl ModelRequest {
    /// Total characters of text across every message.
    ///
    /// Here, once, so that an adapter's token estimate does not mean "the first text part" to
    /// one implementor and "all of them joined" to another.
    #[must_use]
    pub fn text_len(&self) -> u64 {
        let system = self.system.as_deref().map_or(0, |text| {
            u64::try_from(text.chars().count()).unwrap_or(u64::MAX)
        });
        let messages = self
            .messages
            .iter()
            .map(|message| {
                u64::try_from(message.content.text().chars().count()).unwrap_or(u64::MAX)
            })
            .fold(0u64, |total, len| total.saturating_add(len));
        system.saturating_add(messages)
    }

    /// Every knob this request actually set.
    ///
    /// A default is not a request: `temperature = None` says nothing, and an adapter is not obliged
    /// to account for it. What this returns is the set an adapter's [`Plan`] must cover — that
    /// comparison is what makes "the adapter silently ignored it" detectable instead of invisible.
    #[must_use]
    pub fn active_fields(&self) -> Vec<crate::contract::Field> {
        use crate::contract::{Field, Hint, Param};

        let mut fields = Vec::new();
        if self.system.is_some() {
            fields.push(Field::System);
        }
        if !self.messages.is_empty() {
            fields.push(Field::Messages);
        }
        if !self.tools.is_empty() {
            fields.push(Field::Tools);
        }
        // Text is the default, so only asking for something else is a request.
        if self.output != crate::contract::Output::Text {
            fields.push(Field::Output);
        }

        let params = &self.params;
        if params.max_output_tokens.is_some() {
            fields.push(Field::Param(Param::MaxOutputTokens));
        }
        if params.temperature.is_some() {
            fields.push(Field::Param(Param::Temperature));
        }
        if params.top_p.is_some() {
            fields.push(Field::Param(Param::TopP));
        }
        if !params.stop.is_empty() {
            fields.push(Field::Param(Param::Stop));
        }
        if params.reasoning_effort.is_some() {
            fields.push(Field::Param(Param::ReasoningEffort));
        }
        if params.parallel_tool_calls.is_some() {
            fields.push(Field::Param(Param::ParallelToolCalls));
        }
        if params.format != TextFormat::Text {
            fields.push(Field::Param(Param::Format));
        }
        // `ToolChoice::Auto` is the provider default, so only an explicit choice is a request.
        if params.tool_choice != ToolChoice::Auto {
            fields.push(Field::ToolChoice);
        }

        let hints = &self.hints;
        if hints.verbosity.is_some() {
            fields.push(Field::Hint(Hint::Verbosity));
        }
        if hints.reasoning_summary.is_some() {
            fields.push(Field::Hint(Hint::ReasoningSummary));
        }
        if hints.service_tier.is_some() {
            fields.push(Field::Hint(Hint::ServiceTier));
        }
        if hints.max_tool_calls.is_some() {
            fields.push(Field::Hint(Hint::MaxToolCalls));
        }
        if hints.truncation.is_some() {
            fields.push(Field::Hint(Hint::Truncation));
        }
        if !hints.metadata.is_empty() {
            fields.push(Field::Hint(Hint::Metadata));
        }
        fields.extend(self.extensions.active_fields());
        fields
    }
}

/// What an adapter will do with a request, said before it does it.
///
/// The two lists answer one question between them: *did this adapter account for everything the
/// caller asked for?* The kernel compares them against
/// [`ModelRequest::active_fields`] and refuses the call when something was neither honoured nor
/// explained — because a field that silently does nothing is a session behaving differently from how
/// it was configured, and the difference is undiscoverable afterwards.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// Fields this call will honour.
    pub honoured: Vec<crate::contract::Field>,
    /// Fields honoured only in part, with the reason.
    pub degraded: Vec<crate::contract::Degraded>,
}

impl Plan {
    /// A plan that claims every field the request set, for an adapter that has not been taught to
    /// be specific. The check then says nothing about it, which is the state the trait's implementors
    /// were in before it existed.
    #[must_use]
    pub fn assume_all(request: &ModelRequest) -> Self {
        Self {
            honoured: request.active_fields(),
            degraded: Vec::new(),
        }
    }
}

// The generation parameters are data, not adapter behaviour: the checkpoint carries them, so
// they live beside the rest of the contract and are re-exported here for convenience.
pub use crate::contract::{GenerateParams, ReasoningEffort, TextFormat, ToolChoice};

// ---------------------------------------------------------------------------
// Deltas
// ---------------------------------------------------------------------------

/// One fragment of a model's answer.
///
/// The variants mirror the events the kernel emits, but they are not the events: an adapter
/// reports what the provider said, and the driver decides what that means for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelDelta {
    /// Text for a specific part of the message.
    /// A fragment appended to the part at `part_index`.
    ///
    /// One variant for every content kind: a new modality adds no variant here. The merge rule is
    /// carried by the variant, not inferred from the part — appending is what streaming needs, and
    /// replacing is a different thing.
    PartAppend {
        /// Which part of the assistant message this belongs to.
        part_index: u32,
        /// The fragment.
        part: crate::contract::Part,
    },
    /// A whole part at `part_index`, replacing whatever was there.
    PartReplace {
        /// Which part of the assistant message this belongs to.
        part_index: u32,
        /// The part.
        part: crate::contract::Part,
    },
    /// A tool call began. Its arguments follow as [`ModelDelta::ToolCallArgs`].
    ToolCallStarted {
        /// Position in this turn's tool-call list.
        ///
        /// This is the tool-call ordinal, and it is what [`ModelDelta::ToolCallArgs`] and the result
        /// are correlated by. It is not the position of the call in the message.
        index: u32,
        /// Which part of the assistant message this call occupies.
        ///
        /// The same index space as [`ModelDelta::PartAppend`] and [`ModelDelta::PartReplace`]. A tool
        /// call is a part at its own index, not a separate sequence, so text → call → more text
        /// survives assembly in the order it arrived.
        part_index: u32,
        /// The correlation id the result will be filed under.
        id: String,
        /// The tool name.
        name: String,
    },
    /// A fragment of one tool call's arguments.
    ///
    /// Arguments stream as text because that is what providers send, and because a partial
    /// object cannot be handed to a tool. The kernel assembles them by index and validates the
    /// JSON before anything runs.
    ToolCallArgs {
        /// Which call this fragment belongs to.
        index: u32,
        /// The fragment.
        fragment: String,
    },
    /// What the call has consumed so far. A provider may report it more than once.
    Usage(Usage),
    /// Metadata about what answered, known once the response is identified.
    ///
    /// Empty fields mean the provider exposes nothing; a caller that needs to correlate with the
    /// provider's logs should look here rather than at the request it sent.
    Metadata {
        /// The provider's response identifier, when the wire carries one.
        response_id: Option<String>,
        /// The provider's request identifier, when it carries one.
        request_id: Option<String>,
    },
    /// The provider's own reason for stopping.
    Finished {
        /// What it said.
        reason: FinishReason,
    },
}

/// Why the provider says it stopped.
///
/// This is one of four stop conditions and the least trustworthy one: a model that never
/// emits `Stop` must still be stopped by the step cap, the clock or the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    /// Finished an answer.
    Stop,
    /// Wants a tool call.
    ToolCalls,
    /// Hit the completion ceiling.
    Length,
    /// Content was filtered by the provider.
    Filtered,
    /// Something else, passed through uninterpreted.
    Other(String),
}
