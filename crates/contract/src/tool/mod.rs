//! The tool shapes the model sees and produces.
//!
//! Nothing here decides visibility or permission: a definition is what *may* be
//! shown, and a call is what *was* asked for. Whether it runs is a gate's
//! answer, and whether it is shown is a projection's.
//!
//! A result carries content and a failure flag, and no accounting. That is not an omission: a tool
//! that generates media reaches a model through the runtime's session-scoped handle, and the spend
//! is recorded there, when the call returns — before the tool does. A number a tool reported about
//! itself would be a claim the kernel cannot check, and it would be missing exactly in the case that
//! matters, when the tool failed after spending.

mod approval;
mod definition;
mod exposure;
#[cfg(test)]
mod tests;

pub use approval::{Approval, ApprovalOption, ApprovalRequired};
pub use definition::ToolDefinition;
pub use exposure::Exposure;
