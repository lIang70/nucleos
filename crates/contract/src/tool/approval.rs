use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One choice a caller may put in front of a user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalOption {
    /// The stable identifier echoed by [`Approval::option_id`].
    pub id: String,
    /// What the choice means in human-readable form.
    pub label: String,
}

/// A tool call that cannot run until the caller states an approval.
///
/// This is a response value, not a suspended kernel operation. The caller asks its user and
/// resends the call with [`Approval`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalRequired {
    /// The [`crate::ToolCall::id`] that needs approval.
    pub call_id: String,
    /// Why execution needs a user decision.
    pub reason: String,
    /// The choices the caller may present.
    pub options: Vec<ApprovalOption>,
}

/// A stated approval for one call.
///
/// A caller presents these when it resends a batch that came back asking for one. The
/// kernel cannot authenticate a human — it never sees one — so what it *can* do is record
/// that the caller stated an approval for this call, which is exactly as much as parking a
/// session ever proved.
///
/// It lives here, not at the edge, because the kernel's own signature has to name it: a
/// dispatch takes the calls **and** the approvals, and decides which ones still need the
/// gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Approval {
    /// The [`crate::ToolCall::id`] being approved, echoed from [`ApprovalRequired::call_id`].
    pub id: String,
    /// Which offered option was stated.
    pub option_id: String,
}
