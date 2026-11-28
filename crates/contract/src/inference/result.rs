use super::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One inference call's outcome: the identity half and the account half.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Outcome {
    /// What answered, and how to join it to the provider's own logs.
    #[serde(default)]
    pub metadata: Metadata,
    /// What it used and cost.
    #[serde(default)]
    pub usage: Usage,
}

impl Outcome {
    /// When there is no outcome to report.
    #[must_use]
    pub fn unreported(&self) -> bool {
        self == &Self::default()
    }

    /// Check the accounting a reader will rely on.
    ///
    /// # Errors
    ///
    /// A breakdown that exceeds its total: under-reporting leaves the rest unattributed, but
    /// over-reporting is a contradiction.
    pub fn validate(&self) -> crate::error::Result<()> {
        self.usage.validate()
    }
}

/// The provider's own receipt for a call.
///
/// No model here, deliberately. Which model answered is a typed fact that belongs to the response
/// and to the record, because it is what a fallback decision and an authority check read; a name in
/// a metadata bag could be logged but not compared. This struct keeps only what the vendor said.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Metadata {
    /// The provider's request id, when its wire carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// The provider's id for the response object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
}
