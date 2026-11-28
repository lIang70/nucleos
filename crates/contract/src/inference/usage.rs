use super::*;
use crate::error::{Error, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What one call used.
///
/// Totals are **inclusive**: the cache buckets are disjoint subsets of
/// `input_tokens`, and a modality breakdown is a subset of its total. So
/// under-reporting means the rest is unattributed, while over-reporting is a
/// contradiction — which is what [`Usage::validate`] rejects.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Usage {
    /// Prompt tokens. Always normalized by the driver, so it is not optional.
    #[serde(default)]
    pub input_tokens: u64,
    /// Generated tokens.
    #[serde(default)]
    pub output_tokens: u64,
    /// Non-token meters, keyed so a new modality is a new kind rather than
    /// another field. This is the answer to "an image has no token bound".
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub work: BTreeMap<WorkKind, u64>,
    /// The cache split of `input_tokens`.
    #[serde(default)]
    pub input: InputTokenUsage,
    /// The breakdowns that cannot be summed across calls.
    #[serde(default)]
    pub details: Details,
    /// The provider's own price. A price is not a meter: [`Usage::add_meter`]
    /// never touches it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<Billing>,
}

/// A non-token meter.
///
/// The unit is part of the name, so values of different kinds never mix even
/// though they share a representation.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub enum WorkKind {
    /// Images generated.
    #[serde(rename = "images.generated")]
    ImagesGenerated,
    /// Videos generated.
    #[serde(rename = "videos.generated")]
    VideosGenerated,
    /// Characters sent to a speech surface.
    #[serde(rename = "characters.input")]
    InputCharacters,
    /// Generated audio length, in milliseconds.
    #[serde(rename = "audio.duration_ms")]
    AudioDurationMillis,
    /// Generated video length, in milliseconds.
    #[serde(rename = "video.duration_ms")]
    VideoDurationMillis,
}

/// The cache split of the prompt total.
///
/// Every bucket is a subset of the inclusive total, so they sum to at most
/// `input_tokens`. A driver whose wire counts exclusively normalizes on the way
/// in — Anthropic's `input_tokens` excludes cached tokens, so its canonical total
/// is `input_tokens + cache_read + cache_write`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct InputTokenUsage {
    /// Read from the provider's cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Written to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// Neither read nor written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncached_tokens: Option<u64>,
}

/// Whether reasoning tokens are counted inside the output total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningAccounting {
    /// `reasoning_tokens` is a subset of `output_tokens`.
    #[default]
    IncludedInOutput,
    /// The provider accounts for reasoning **besides** the output total.
    Additional,
    /// A count was reported, but its inclusion is undocumented. Kept rather
    /// than dropped: a number whose meaning is unknown is still evidence.
    Unknown,
}

/// Output-side detail whose sum semantics depend on the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct OutputTokenUsage {
    /// Reasoning tokens, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// How those tokens relate to the output total.
    #[serde(default)]
    pub reasoning_accounting: ReasoningAccounting,
}

impl Usage {
    /// Prompt plus generated tokens. Derived, so it can never disagree with its
    /// parts.
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// One meter, or zero when it was not reported.
    #[must_use]
    pub fn work_value(&self, kind: WorkKind) -> u64 {
        self.work.get(&kind).copied().unwrap_or(0)
    }

    /// Sum the meters a budget accumulator sums.
    ///
    /// Token totals, the input-side cache counters and the typed work meters —
    /// and deliberately **not** `details` or `billing`: a breakdown and a price
    /// have no provider-independent sum, and a price describes one call rather
    /// than an amount to add up.
    #[must_use]
    pub fn add_meter(&self, other: &Self) -> Self {
        let mut work = self.work.clone();
        for (kind, value) in &other.work {
            let entry = work.entry(*kind).or_insert(0);
            *entry = entry.saturating_add(*value);
        }
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            work,
            input: InputTokenUsage {
                cache_read_tokens: sum_options(
                    self.input.cache_read_tokens,
                    other.input.cache_read_tokens,
                ),
                cache_write_tokens: sum_options(
                    self.input.cache_write_tokens,
                    other.input.cache_write_tokens,
                ),
                uncached_tokens: sum_options(
                    self.input.uncached_tokens,
                    other.input.uncached_tokens,
                ),
            },
            // A breakdown and a price are not meters.
            details: Details::default(),
            billing: None,
        }
    }

    /// Fold another report in, price included.
    ///
    /// An unknown price stays unknown unless **both** sides reported one, in the
    /// same currency and scale: one known half does not make an unknown whole
    /// known, and a currency conversion is not this type's decision.
    #[must_use]
    pub fn fold(&self, other: &Self) -> Self {
        let mut total = self.add_meter(other);
        total.billing = match (&self.billing, &other.billing) {
            (Some(mine), Some(theirs)) => Some(Billing {
                input_tokens: sum_options(mine.input_tokens, theirs.input_tokens),
                output_tokens: sum_options(mine.output_tokens, theirs.output_tokens),
                cost: match (&mine.cost, &theirs.cost) {
                    (Some(a), Some(b)) => a.checked_add(b),
                    _ => None,
                },
            }),
            _ => None,
        };
        total
    }

    /// The consistency rules, run where a response is validated rather than on
    /// every read.
    ///
    /// Two shapes: a bucket may not exceed the total it is a subset of, and a
    /// breakdown may not exceed its total either. Over-reporting is a
    /// contradiction; under-reporting is merely unattributed.
    pub fn validate(&self) -> Result<()> {
        // Not `sum_options`: an unknown half must not hide a known bucket. Summing two meters keeps
        // an unknown, but checking a bound only needs the parts that were reported.
        let cache_sum = known_sum(self.input.cache_read_tokens, self.input.cache_write_tokens);
        if cache_sum > self.input_tokens {
            return Err(Error::invalid(format!(
                "cache buckets ({cache_sum}) exceed the prompt total ({})",
                self.input_tokens
            )));
        }
        for (modality, count) in &self.details.input_modalities {
            if *count > self.input_tokens {
                return Err(Error::invalid(format!(
                    "{modality:?} input tokens ({count}) exceed the prompt total ({})",
                    self.input_tokens
                )));
            }
        }
        for (modality, count) in &self.details.output_modalities {
            if *count > self.output_tokens {
                return Err(Error::invalid(format!(
                    "{modality:?} output tokens ({count}) exceed the generated total ({})",
                    self.output_tokens
                )));
            }
        }
        if self.details.output.reasoning_accounting == ReasoningAccounting::IncludedInOutput
            && self
                .details
                .output
                .reasoning_tokens
                .is_some_and(|tokens| tokens > self.output_tokens)
        {
            return Err(Error::invalid(
                "reasoning tokens exceed the generated total they are declared to be inside",
            ));
        }
        Ok(())
    }
}

/// The sum of what was reported, treating an unreported half as absent rather than unknown.
///
/// [`sum_options`] keeps an unknown half unknown because adding an unknown as zero is a quieter
/// lie. That rule is right for a total and wrong for a bound: a cache bucket that *was* reported and
/// exceeds the prompt total is a contradiction whether or not its sibling was reported.
fn known_sum(left: Option<u64>, right: Option<u64>) -> u64 {
    left.unwrap_or(0).saturating_add(right.unwrap_or(0))
}

fn sum_options(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        // One unknown half keeps the whole unknown: adding an unknown as zero is
        // the same lie told more quietly.
        _ => None,
    }
}
