use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::content::{ToolCall, ToolResult};
use crate::tool::{Approval, ApprovalRequired, ToolDefinition};

/// The tools currently visible to one session.
///
/// Visibility has already been filtered by the session plan and each tool's exposure. This is a
/// catalogue, not an execution grant: every call is checked against the current envelope again.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ToolCatalog {
    /// Definitions in the order the caller should present them.
    pub tools: Vec<ToolDefinition>,
}

/// One batch of native tool calls.
///
/// When an earlier response returned `approvals_required`, the caller resends the same calls with
/// the corresponding approvals. The runtime does not park a session or retain a pending question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CallTools {
    /// Calls to dispatch, in batch order.
    pub calls: Vec<ToolCall>,
    /// User decisions stated for calls in this batch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvals: Vec<Approval>,
}

/// What a batch did, and what still needs a user decision.
///
/// A call that needs approval has an [`ApprovalRequired`] entry rather than a fabricated result.
/// Execution failures are ordinary [`ToolResult`] values with `is_error = true`, so the model can
/// read the failure on the next inference call.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ToolBatchResult {
    /// Results for calls that actually ran.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub results: Vec<ToolResult>,
    /// Calls that did not run because they need approval.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub approvals_required: Vec<ApprovalRequired>,
}

// Results are deliberately not merged into server-side conversation state. The caller turns them
// into tool messages and includes the complete conversation in its next `ModelRequest`.
