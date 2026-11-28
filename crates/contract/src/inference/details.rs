use super::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Which modality a breakdown counts.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    /// Text.
    Text,
    /// Images.
    Image,
    /// Audio.
    Audio,
    /// Video.
    Video,
    /// A non-media file.
    File,
}

/// How long a cache write is kept for.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub enum CacheTtl {
    /// Five minutes.
    #[serde(rename = "5m")]
    FiveMinutes,
    /// One hour.
    #[serde(rename = "1h")]
    OneHour,
}

/// The non-summable half of one call's account.
///
/// It has no `add`: a caller that wants to accumulate a breakdown does so
/// explicitly, because a breakdown and a price have no provider-independent sum
/// semantics.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Details {
    /// Input tokens per modality.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub input_modalities: BTreeMap<Modality, u64>,
    /// Output tokens per modality.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub output_modalities: BTreeMap<Modality, u64>,
    /// Cache writes per TTL.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cache_writes: BTreeMap<CacheTtl, u64>,
    /// Output-side detail.
    #[serde(default)]
    pub output: OutputTokenUsage,
}
