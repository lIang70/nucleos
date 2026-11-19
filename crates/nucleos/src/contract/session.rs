//! Where a session can be, why it stopped, and what it is allowed to consume.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Where a session is in its life.
///
/// The vocabulary is the session's, not a provider's. `AwaitingHuman` is a state rather
/// than a callback, so a confirmation that arrives three hours later resumes a session
/// instead of reviving a stack frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Accepting calls.
    Open,
    /// Finished. No further call is accepted.
    Closed,
}

impl SessionState {
    /// Whether the session accepts no further calls.
    #[must_use]
    pub fn is_closed(self) -> bool {
        matches!(self, Self::Closed)
    }
}

/// Why a session stopped.
///
/// Four layers, all present: a provider's `finish_reason`, the step cap, the wall clock,
/// and the cost ledger. Each alone can be defeated; together they cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model said it was finished and no stop condition fired first.
    Completed,
    /// The step cap was reached.
    Steps,
    /// The token budget was spent.
    Tokens,
    /// The cost budget was spent.
    Cost,
    /// The wall-clock budget was spent.
    Wall,
    /// Policy denied a call.
    Denied,
    /// A tool or model call failed.
    Failed,
    /// Stopped on request.
    Cancelled,
}

/// What a session may consume.
///
/// `None` means unbounded for that dimension, which is a decision the host makes, not a
/// default the kernel assumes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Budget {
    /// Turns a session may take.
    pub max_steps: Option<u64>,
    /// Tokens across every model call, including failed attempts that streamed.
    pub max_tokens: Option<u64>,
    /// Cost in micro-USD. Integer, because money in a float is a rounding argument waiting
    /// to happen.
    pub max_micros_usd: Option<u64>,
    /// Wall clock for the whole session.
    pub max_wall_millis: Option<u64>,
}
