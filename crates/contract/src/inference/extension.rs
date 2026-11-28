//! Provider-specific options, and the coordinates for reporting on them.
//!
//! An extension is a JSON value whose *schema* only one provider understands.
//! The kernel stores, patches, merges and forwards it — and never reads inside
//! it. That is the whole rule: a field the kernel must **read** is a contract
//! field; a field it merely **forwards** is an extension.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A provider-qualified extension id, `vendor/name` — the same shape as a model
/// reference, because both name something one vendor owns.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct ExtensionId(String);

impl ExtensionId {
    /// Parse `vendor/name`.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        match value.split_once('/') {
            Some((vendor, name)) if !vendor.is_empty() && !name.is_empty() => Ok(Self(value)),
            _ => Err(Error::invalid(format!(
                "an extension id is `vendor/name`, got {value:?}"
            ))),
        }
    }

    /// The vendor half.
    #[must_use]
    pub fn vendor(&self) -> &str {
        self.0.split_once('/').map_or("", |(vendor, _)| vendor)
    }

    /// The vendor and name, as one string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The provider-specific options of one call.
///
/// Validated at **write** time against the registry of this build: an id this
/// build cannot decode is an error, not a value that is quietly ignored —
/// "configured but silently ineffective" is the worst state a knob can be in.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ExtensionSet(pub std::collections::BTreeMap<ExtensionId, serde_json::Value>);

impl ExtensionSet {
    /// Empty.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether there is nothing here.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// One provider's options.
    #[must_use]
    pub fn get(&self, id: &ExtensionId) -> Option<&serde_json::Value> {
        self.0.get(id)
    }

    /// Overlay `other` on top of this set, id by id.
    ///
    /// Per-call options win per id, not per field: merging two providers'
    /// objects field by field would need a schema for something whose schema is
    /// exactly what the kernel does not know.
    #[must_use]
    pub fn overlay(&self, other: &Self) -> Self {
        let mut merged = self.clone();
        for (id, value) in &other.0 {
            merged.0.insert(id.clone(), value.clone());
        }
        merged
    }
}

/// Which knob a report is about.
///
/// One vocabulary for warnings and errors, so "the output was adjusted" and "the
/// tools were dropped" point at the same coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "field", rename_all = "snake_case")]
pub enum Field {
    /// The model reference.
    Model,
    /// The conversation sent.
    Messages,
    /// The per-call instructions.
    Instructions,
    /// What to produce.
    Output,
    /// The tool set offered.
    Tools,
    /// The tool choice constraint.
    ToolChoice,
    /// The output token ceiling.
    MaxOutputTokens,
    /// One provider's options.
    Extension {
        /// Which one.
        id: ExtensionId,
    },
}

/// A requested field the provider could not honour **in full**.
///
/// Informational: the response is still usable. A field that could not be
/// honoured *at all* is an error, not a warning — otherwise "degraded" becomes a
/// bucket that hides refusals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Warning {
    /// Which knob was adjusted.
    pub field: Field,
    /// A stable machine label ("quantized", "not_supported", "implicit_off").
    pub code: String,
    /// The explanation a human reads.
    pub message: String,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn an_extension_id_needs_both_halves() {
        assert!(ExtensionId::new("openai/generation").is_ok());
        assert!(ExtensionId::new("generation").is_err());
        assert!(ExtensionId::new("openai/").is_err());
    }

    #[test]
    fn overlay_replaces_whole_ids() {
        let base = ExtensionSet(
            [(
                ExtensionId::new("openai/generation").unwrap(),
                serde_json::json!({"temperature": 0.2, "top_p": 0.9}),
            )]
            .into_iter()
            .collect(),
        );
        let call = ExtensionSet(
            [(
                ExtensionId::new("openai/generation").unwrap(),
                serde_json::json!({"temperature": 0.0}),
            )]
            .into_iter()
            .collect(),
        );
        let merged = base.overlay(&call);
        let value = merged
            .get(&ExtensionId::new("openai/generation").unwrap())
            .unwrap();
        assert_eq!(value, &serde_json::json!({"temperature": 0.0}));
    }

    #[test]
    fn a_set_serialises_as_a_plain_object() {
        let set = ExtensionSet(
            [(
                ExtensionId::new("openai/generation").unwrap(),
                serde_json::json!({"temperature": 0.2}),
            )]
            .into_iter()
            .collect(),
        );
        let json = serde_json::to_string(&set).unwrap();
        assert_eq!(json, r#"{"openai/generation":{"temperature":0.2}}"#);
    }
}
