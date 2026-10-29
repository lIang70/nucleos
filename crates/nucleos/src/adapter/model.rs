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

    /// Start a call.
    ///
    /// An error here means the call never started — a bad request, a refused credential, an
    /// unreachable provider. An error *inside* the stream means it started and then failed,
    /// which is the case that still owes money and still has to be recorded.
    async fn invoke(&self, request: ModelRequest) -> Result<ModelStream>;
}

/// Everything the kernel sends.
///
/// `PartialEq` but not `Eq`: sampling parameters are floats, and a float has no total order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelRequest {
    /// Standing instructions, if the session has any.
    pub system: Option<String>,
    /// The conversation, oldest first.
    pub messages: Vec<Message>,
    /// The tools this turn may use.
    pub tools: Vec<ToolSpec>,
    /// How to shape the answer.
    pub params: GenerateParams,
    /// Why this call is happening.
    pub purpose: Purpose,
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
    Text {
        /// Which part of the assistant message this belongs to.
        part_index: u32,
        /// The fragment.
        text: String,
    },
    /// Reasoning for a specific part, and/or the provider's signature.
    ///
    /// A signature usually arrives on its own, with empty `text`: providers sign the
    /// finished block rather than each fragment.
    Reasoning {
        /// Which part of the assistant message this belongs to.
        part_index: u32,
        /// The fragment, possibly empty.
        text: String,
        /// The provider's signature fragment, returned verbatim.
        signature: String,
    },
    /// A tool call began. Its arguments follow as [`ModelDelta::ToolCallArgs`].
    ToolCallStarted {
        /// Position in this turn's tool-call list.
        index: u32,
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
    /// A field the provider could not honour.
    ///
    /// Reported, never swallowed: a `tool_choice = Required` that quietly became `Auto` is a
    /// session behaving differently than it was configured to.
    Dropped {
        /// The request field, e.g. `tool_choice`.
        field: &'static str,
        /// Why, in the provider's terms.
        reason: String,
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
