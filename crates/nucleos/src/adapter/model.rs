//! The model, and the shapes a provider adapter has to speak.
//!
//! The kernel knows one request shape and one delta shape. A provider with more concepts than
//! these lowers them on the way in and loses them on the way out — deliberately, because the
//! alternative is a union of every provider's vocabulary, which is the thing this kernel
//! exists not to be.

use crate::contract::{Capability, Message, ToolCall, ToolSpec, Usage};
use crate::error::Result;
use async_trait::async_trait;
use std::pin::Pin;
use tokio_stream::Stream;

/// A stream of deltas.
///
/// `Send` so the engine can drive a session from any task; `'static` so it can outlive the
/// call that produced it.
pub type ModelStream = Pin<Box<dyn Stream<Item = Result<ModelDelta>> + Send>>;

/// One model, behind the only interface the kernel has.
#[async_trait]
pub trait Model: Send + Sync {
    /// What this model can do. Read at `Start`, never discovered by trial.
    fn capability(&self) -> Capability;

    /// Start a call.
    ///
    /// An error here means the call never started — a bad request, a refused credential, an
    /// unreachable provider. An error *inside* the stream means it started and then failed,
    /// which is the case that still owes money and still has to be recorded.
    async fn invoke(&self, request: ModelRequest) -> Result<ModelStream>;
}

/// Everything the kernel sends.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelRequest {
    /// Standing instructions, if the session has any.
    pub system: Option<String>,
    /// The conversation, oldest first.
    pub messages: Vec<Message>,
    /// The tools this turn may use.
    pub tools: Vec<ToolSpec>,
    /// The completion ceiling for this turn, already clamped against the session budget.
    pub max_output_tokens: Option<u32>,
}

/// One fragment of a model's answer.
///
/// The variants mirror the events the kernel emits, but they are not the events: an adapter
/// reports what the provider said, and the engine decides what that means for a session.
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
    /// The model asking for a tool.
    ToolCall(ToolCall),
    /// What the call has consumed so far. A provider may report it more than once.
    Usage(Usage),
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
