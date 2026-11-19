//! The two identity primitives: a session name and a tool-set number.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A session identifier.
///
/// The caller chooses it when resuming, so it is a string rather than an opaque handle.
/// That makes a retried `Start` idempotent instead of creating a second session.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub struct SessionId(String);

impl SessionId {
    /// Wrap a caller-supplied identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A tool-source generation.
///
/// Generations are immutable snapshots of the declared sources. A session is pinned to
/// one at start and may only move forward at a turn boundary, and only when the two
/// generations are compatible.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
)]
pub struct Generation(pub u64);
