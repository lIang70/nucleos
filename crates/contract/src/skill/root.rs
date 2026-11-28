use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A deployment-declared skill root.
///
/// It is a name such as `engineering`, not `/srv/skills/engineering`: resolving that name to a
/// path is exclusively the deployment's business.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(try_from = "String")]
pub struct SkillRootName(String);

impl SkillRootName {
    /// Validate and wrap a root name.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() {
            return Err(Error::invalid("a skill root name must not be empty"));
        }
        if value.len() > 128 {
            return Err(Error::invalid(
                "a skill root name must be at most 128 bytes",
            ));
        }
        if value.chars().any(|character| {
            character == '/' || character.is_whitespace() || character.is_control()
        }) {
            return Err(Error::invalid(
                "a skill root name must not contain '/', whitespace, or control characters",
            ));
        }
        Ok(Self(value))
    }

    /// The declared name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SkillRootName {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl std::fmt::Display for SkillRootName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
