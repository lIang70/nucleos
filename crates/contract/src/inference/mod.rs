//! The model seam: what a request names, what a model declares, what comes back, and what it cost.
//!
//! A call names a model ([`id`]), the model declares what it can do ([`model`]), provider options
//! ride beside the request without the kernel reading them ([`extension`]), and the answer arrives
//! with the accounting ([`Usage`]) attached to it.
//!
//! Accounting lives here rather than in a module of its own because nothing else has a price. A tool
//! spends wall-clock time inside a sandbox boundary and reports no meter; a session's spend is the
//! sum of its model calls. A separate `outcome` module invited the belief that a tool could report
//! what it cost, which was never true.
//!
//! What is deliberately absent: routing, retry, session lifecycle and transport framing. Those
//! belong to the runtime and wire; here there is only the vocabulary both sides must speak.

mod decision;
mod details;
mod embedding;
pub mod extension;
mod generation;
pub mod id;
pub mod model;
mod money;
mod result;
#[cfg(test)]
mod tests;
mod usage;

pub use decision::{
    DecideRequest, Decision, DecisionResponse, Probability, Question, QuestionKind,
};
pub use details::{CacheTtl, Details, Modality};
pub use embedding::{EmbedRequest, EmbedResponse};
pub use extension::{ExtensionId, ExtensionSet, Field, Warning};
pub use generation::{GenerateResponse, ModelRequest, StreamEvent};
pub use id::{ModelId, ModelRef};
pub use model::{Capabilities, FinishReason, Lifecycle, Limits, Mode, ModelDescriptor, Output};
pub use money::{Billing, Money};
pub use result::{Metadata, Outcome};
pub use usage::{InputTokenUsage, OutputTokenUsage, ReasoningAccounting, Usage, WorkKind};
