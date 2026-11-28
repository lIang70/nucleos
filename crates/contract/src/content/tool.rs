use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The validated name shared by a tool definition, a tool call, and its authority.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(try_from = "String")]
pub struct ToolName(String);

impl ToolName {
    /// Validate and wrap a tool name.
    ///
    /// The contract only excludes names that cannot cross provider and wire boundaries safely.
    /// A provider with a narrower alphabet validates that in its adapter.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() {
            return Err(Error::invalid("a tool name must not be empty"));
        }
        if value.len() > 128 {
            return Err(Error::invalid("a tool name must be at most 128 bytes"));
        }
        if value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err(Error::invalid(
                "a tool name must not contain whitespace or control characters",
            ));
        }
        Ok(Self(value))
    }

    /// The registered name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ToolName {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl std::fmt::Display for ToolName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
