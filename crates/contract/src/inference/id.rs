//! What names a model, and nothing else.
//!
//! A model reference is `vendor/name` — one string, because a request must be answerable by
//! exactly one provider. A bare name would need a registry lookup whose answer could differ
//! between two deployments that both serve `gpt-5`.
//!
//! The rest of the old identifier module — session ids, principals, sequence numbers — went
//! with the layers that owned them: they are the runtime's nouns, not an adapter's.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A model's own name, without the vendor.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct ModelId(String);

impl ModelId {
    /// Wrap a name.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() || value.contains('/') {
            return Err(Error::invalid(
                "a model id is a bare name; the vendor lives in ModelRef",
            ));
        }
        Ok(Self(value))
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A model as a request names it: `vendor/name`.
///
/// One string because a request must be answerable by exactly one provider: a
/// bare name would need a registry lookup whose answer could differ between two
/// deployments that both serve `gpt-5`.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct ModelRef(String);

impl ModelRef {
    /// Parse `vendor/name`.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        match value.split_once('/') {
            Some((vendor, name)) if !vendor.is_empty() && !name.is_empty() => Ok(Self(value)),
            _ => Err(Error::invalid(format!(
                "a model reference is `vendor/name`, got {value:?}"
            ))),
        }
    }

    /// The vendor half.
    #[must_use]
    pub fn vendor(&self) -> &str {
        self.0.split_once('/').map_or("", |(vendor, _)| vendor)
    }

    /// The model half.
    #[must_use]
    pub fn name(&self) -> &str {
        self.0.split_once('/').map_or(&self.0, |(_, name)| name)
    }

    /// The whole reference.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ModelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_model_ref_needs_both_halves() {
        assert!(ModelRef::new("openai/gpt-5").is_ok());
        assert!(ModelRef::new("gpt-5").is_err());
        assert!(ModelRef::new("/gpt-5").is_err());
        assert!(ModelRef::new("openai/").is_err());
    }

    #[test]
    fn a_model_id_is_a_bare_name() {
        assert!(ModelId::new("gpt-5").is_ok());
        assert!(
            ModelId::new("openai/gpt-5").is_err(),
            "the vendor lives in ModelRef"
        );
    }
}
