use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Why a process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// A normal exit, including a non-zero code.
    Exited,
    /// The OS reported death by signal.
    Signaled,
    /// A terminate call stopped it.
    Terminated,
    /// The timeout elapsed and it was killed.
    TimedOut,
    /// The backend lost it — the handle outlived the process it described.
    Lost,
}

/// How one process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionExit {
    /// The exit code, when it exited normally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    /// The signal, when a signal ended it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// Which of the two, and why.
    pub reason: ExitReason,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
