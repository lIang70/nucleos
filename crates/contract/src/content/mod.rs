//! What travels between a caller and a model.
//!
//! A message is a role plus ordered parts. The parts are the union that both
//! sides of every seam agree on: a tool result is as much a part as a paragraph,
//! which is why a failed call needs no second error channel — the text the model
//! must read lives in a part.

mod arguments;
mod body;
mod conversation;
mod message;
mod part;
#[cfg(test)]
mod tests;
mod tool;

pub use arguments::{Arguments, ToolCall, ToolResult};
pub use body::Content;
pub use conversation::{validate_callable, validate_well_formed};
pub use message::{Message, Metadata, Role};
pub use part::{MediaSource, Part, PartKind};
pub use tool::ToolName;
