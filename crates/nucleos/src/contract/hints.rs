//! Parameters that belong to a session rather than to one call.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Preferences an adapter may honour, and must report when it cannot.
///
/// Separate from [`GenerateParams`] because these change while a session runs — a conversation
/// that is filling its window wants less prose, and a review turn wants more detail — and because
/// they are *hints*: an adapter that has no channel for one reports a [`Degraded`] rather than
/// failing the call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Hints {
    /// How much prose an answer should carry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<Verbosity>,
    /// How much of the reasoning to summarise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_summary: Option<Summary>,
    /// The cost/latency trade this session prefers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    /// A ceiling on tool calls in one response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    /// What to do when the conversation does not fit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<Truncation>,
    /// Labels the provider echoes back, up to sixteen.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

impl Hints {
    /// A patch that leaves every field `None` changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Take every field this patch sets, and keep the rest.
    #[must_use]
    pub fn merged(&self, over: &Self) -> Self {
        Self {
            verbosity: over.verbosity.or(self.verbosity),
            reasoning_summary: over.reasoning_summary.or(self.reasoning_summary),
            service_tier: over.service_tier.or(self.service_tier),
            max_tool_calls: over.max_tool_calls.or(self.max_tool_calls),
            truncation: over.truncation.or(self.truncation),
            metadata: if over.metadata.is_empty() {
                self.metadata.clone()
            } else {
                over.metadata.clone()
            },
        }
    }
}

/// How much prose an answer should carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verbosity {
    /// Terse.
    Low,
    /// Normal.
    Medium,
    /// Expansive.
    High,
}

/// How much of the reasoning to summarise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Summary {
    /// The provider decides.
    Auto,
    /// Short.
    Concise,
    /// Long.
    Detailed,
}

/// The cost/latency trade a session prefers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    /// Let the provider decide.
    Auto,
    /// The default tier.
    Default,
    /// Cheaper, slower.
    Flex,
    /// More expensive, faster.
    Priority,
}

/// What to do when the conversation does not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Truncation {
    /// Drop the oldest items.
    Auto,
    /// Fail instead of dropping silently.
    Disabled,
}
