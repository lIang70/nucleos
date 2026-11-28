use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The maximum byte length of an application or session name.
pub const MAX_NAME_LEN: usize = 128;

fn validate(kind: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(Error::invalid(format!("{kind} must not be empty")));
    }
    if value.len() > MAX_NAME_LEN {
        return Err(Error::invalid(format!(
            "{kind} must be at most {MAX_NAME_LEN} bytes"
        )));
    }
    if value
        .chars()
        .any(|character| character == '/' || character.is_whitespace() || character.is_control())
    {
        return Err(Error::invalid(format!(
            "{kind} must not contain '/', whitespace, or control characters"
        )));
    }
    Ok(())
}

/// The application a credential belongs to.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(try_from = "String")]
pub struct AppId(String);

impl AppId {
    /// Validate and wrap an application name.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate("an application id", &value)?;
        Ok(Self(value))
    }

    /// The name exactly as the caller supplied it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AppId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl std::fmt::Display for AppId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A caller-chosen session name, unique within one application.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(try_from = "String")]
pub struct SessionId(String);

impl SessionId {
    /// Validate and wrap a session name.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate("a session id", &value)?;
        Ok(Self(value))
    }

    /// The name exactly as the caller supplied it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SessionId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The complete, unambiguous identity of a session.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub struct SessionKey {
    /// Which application owns it.
    pub app: AppId,
    /// Which of that application's sessions it is.
    pub session: SessionId,
}

impl SessionKey {
    /// Assemble a key from its two coordinates.
    #[must_use]
    pub fn new(app: AppId, session: SessionId) -> Self {
        Self { app, session }
    }
}
