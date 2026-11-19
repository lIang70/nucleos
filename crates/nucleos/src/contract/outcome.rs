//! What one inference call returned: the identity of the call and its account.
//!
//! These two share a module because they are filled at the same moment, by the same party — the
//! adapter, when the stream ends — and read together: every result carries both as adjacent
//! fields, and so does a durable call record. Splitting them would be splitting one concept.
//!
//! The module is a leaf. Three layers name these types without wanting the provider contract: an
//! adapter fills them in, the kernel writes them into an event, and a record reader reads them
//! back. Nothing here imports the kernel or an adapter.
//!
//! # The two rules everything else depends on
//!
//! **A counter that was not reported stays unreported.** Every optional counter is an `Option`, and
//! `None` plus `None` is `None` — never `Some(0)`. "We do not know" and "the provider said zero"
//! are different facts, and a ledger that cannot tell them apart will eventually charge for
//! silence.
//!
//! **A meter and a price are not the same thing.** [`Usage::add_meter`] sums token totals, the
//! input-side cache counters and the typed [`WorkKind`] meters. It does not touch [`Details`] or
//! [`Billing`]: a breakdown and a price have no provider-independent sum semantics. Cost has one
//! home, [`Billing::cost`], so there is never a second field to disagree with it.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The identity half of one call's outcome: what answered, and can the answer be joined to the
/// provider's own logs.
///
/// It carries no payload. The request, the assistant turn and the produced parts are the response's
/// business; this is what a record, a span or a dashboard needs to say "this is the call that came
/// back". The accounting half is [`Usage`], and the two travel together.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Metadata {
    /// The provider-qualified model that produced the response, after routing.
    pub model: String,
    /// The provider-assigned request identifier, when the wire carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// The provider-assigned identifier of the response object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
}

/// The account of one call: token totals, typed work meters, the cache split, the non-summable
/// breakdowns, and [`Billing`] for provider-reported cost.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Usage {
    /// Inclusive prompt tokens.
    pub input_tokens: u64,
    /// Generated tokens.
    pub output_tokens: u64,
    /// Non-token meters, keyed so a new modality is a new constant rather than another field.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub work: BTreeMap<WorkKind, u64>,
    /// The cache split of `input_tokens`. Every bucket is a subset of the inclusive prompt total,
    /// so the buckets sum to at most `input_tokens` — they are not additions to it.
    #[serde(default)]
    pub input: InputTokenUsage,
    /// The breakdowns that cannot be summed across calls. `add_meter` never touches them.
    #[serde(default)]
    pub details: Details,
    /// Provider-reported billed units and exact cost. A price, not a meter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<Billing>,
}

/// The input-side cache counters, each a disjoint subset of the inclusive prompt total.
///
/// A provider whose counters are *exclusive* normalises on the way in: Anthropic's `input_tokens`
/// counts only the tokens that were neither read from nor written to the cache, so its canonical
/// total is `input_tokens + cache_read + cache_write` and `uncached_tokens` carries the exclusive
/// bucket unchanged.
///
/// `None` means "zero, or not reported": a driver omits cache read / write when the provider
/// reports zero, so a missing bucket is inferred from the total rather than from its siblings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InputTokenUsage {
    /// Tokens read from a provider-side cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Tokens written into a provider-side cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// Tokens that were neither read from nor written to the cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncached_tokens: Option<u64>,
}

/// Output-side detail whose sum semantics depend on the provider. A detail, not a meter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OutputTokenUsage {
    /// Tokens the provider spent on reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// How `reasoning_tokens` relates to `output_tokens`.
    #[serde(default)]
    pub reasoning_accounting: ReasoningAccounting,
}

/// Whether reasoning tokens are part of the output total or additional to it.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningAccounting {
    /// `reasoning_tokens` is a subset of `output_tokens`.
    IncludedInOutput,
    /// The provider accounts for reasoning separately from `output_tokens`.
    Additional,
    /// A count was reported but its inclusion semantics are undocumented.
    #[default]
    Unknown,
}

/// The non-summable half of one call's account. It has no `add`: a caller that wants to accumulate
/// a breakdown does so explicitly, and says which side of the ledger it belongs on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Details {
    /// Input tokens by modality. A subset of `input_tokens`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub input_modalities: BTreeMap<Modality, u64>,
    /// Output tokens by modality. A subset of `output_tokens`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub output_modalities: BTreeMap<Modality, u64>,
    /// Cache writes by time-to-live. Their sum must equal `input.cache_write_tokens` when both are
    /// present, or the two reports disagree.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cache_writes: BTreeMap<CacheTtl, u64>,
    /// Output-side detail.
    #[serde(default)]
    pub output: OutputTokenUsage,
}

/// A content modality, for breaking a token total down by kind.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    /// Text.
    Text,
    /// Image.
    Image,
    /// Audio.
    Audio,
    /// Video.
    Video,
    /// A referenced file.
    File,
}

/// A cache's time-to-live bucket.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
pub enum CacheTtl {
    /// Five minutes.
    #[serde(rename = "5m")]
    FiveMinutes,
    /// One hour.
    #[serde(rename = "1h")]
    OneHour,
}

/// A non-token meter. The unit is part of the name, so values of different kinds never mix even
/// though they share a `u64` representation.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
pub enum WorkKind {
    /// Generated images.
    #[serde(rename = "images.generated")]
    ImagesGenerated,
    /// Generated videos.
    #[serde(rename = "videos.generated")]
    VideosGenerated,
    /// Characters sent to a speech surface.
    #[serde(rename = "characters.input")]
    InputCharacters,
    /// Generated audio duration, in milliseconds.
    #[serde(rename = "audio.duration_ms")]
    AudioDurationMillis,
    /// Generated video duration, in milliseconds.
    #[serde(rename = "video.duration_ms")]
    VideoDurationMillis,
}

/// Provider-reported billed units and exact cost, kept apart from the raw token counts so a
/// provider that bills differently from what it counts cannot be silently reconciled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Billing {
    /// Tokens the provider says it billed for the prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Tokens the provider says it billed for the generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// The exact price, with its own currency and scale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Money>,
}

/// `units / 10^scale` in `currency`, so a provider's 10^-10 USD ticks survive without floating
/// point. The scale is the provider's own, deliberately not normalised: converting between scales
/// is a policy decision about rounding, and a type that normalised silently would be guessing at it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Money {
    /// The currency, e.g. `USD`.
    pub currency: String,
    /// The numerator.
    pub units: i64,
    /// The base-10 exponent of the denominator.
    pub scale: u8,
}

impl Money {
    /// Sum two amounts that share a currency and a scale.
    ///
    /// `None` for a mismatch: converting between currencies or scales is a policy decision about
    /// exchange rates and precision, and this type does not make it. A caller enforcing a single
    /// budget currency should normalise or refuse rather than drop the amount.
    #[must_use]
    pub fn add(&self, other: &Self) -> Option<Self> {
        if self.currency != other.currency || self.scale != other.scale {
            return None;
        }
        Some(Self {
            currency: self.currency.clone(),
            scale: self.scale,
            units: self.units.saturating_add(other.units),
        })
    }
}

impl Usage {
    /// The inclusive prompt plus generated total. Derived, so it can never disagree with its parts.
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Sum the meters a budget accumulator sums: token totals, the input-side cache counters and
    /// the typed work meters.
    ///
    /// `Details` and `Billing` are deliberately untouched: a breakdown and a price have no
    /// provider-independent sum semantics, and folding them in would invent one. An optional counter
    /// that is `None` on both sides stays `None` — "not reported" never becomes a reported zero.
    pub fn add_meter(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        merge_work(&mut self.work, &other.work);
        add_optional(
            &mut self.input.cache_read_tokens,
            other.input.cache_read_tokens,
        );
        add_optional(
            &mut self.input.cache_write_tokens,
            other.input.cache_write_tokens,
        );
        add_optional(&mut self.input.uncached_tokens, other.input.uncached_tokens);
    }

    /// One meter, or zero when it was not reported.
    #[must_use]
    pub fn work_value(&self, kind: WorkKind) -> u64 {
        self.work.get(&kind).copied().unwrap_or(0)
    }

    /// Accumulate one meter.
    pub fn add_work(&mut self, kind: WorkKind, value: u64) {
        *self.work.entry(kind).or_insert(0) += value;
    }

    /// A usage that carries a price in micro-USD.
    ///
    /// For tests and for adapters whose provider reports cost in that unit: `Money { currency:
    /// "USD", units: micros, scale: 6 }`, with no other meters set. A price is not a meter, so this
    /// is a constructor, not a fold.
    #[must_use]
    pub fn costing(micros_usd: u64) -> Self {
        Self {
            billing: Some(Billing {
                cost: Some(Money {
                    currency: "USD".to_owned(),
                    units: i64::try_from(micros_usd).unwrap_or(i64::MAX),
                    scale: 6,
                }),
                ..Billing::default()
            }),
            ..Self::default()
        }
    }

    /// The cost in micro-USD, when it is known in that unit.
    ///
    /// A cost ceiling is expressed in micro-USD, so the comparison needs one unit. A report in
    /// another currency or at another scale returns `None` rather than being converted: converting
    /// is a policy decision about rounding, and a ceiling that cannot be evaluated must not fire —
    /// which is the same rule an unreported cost follows.
    #[must_use]
    pub fn cost_micros_usd(&self) -> Option<u64> {
        let cost = self.billing.as_ref()?.cost.as_ref()?;
        if cost.currency == "USD" && cost.scale == 6 {
            Some(cost.units.max(0) as u64)
        } else {
            None
        }
    }

    /// The consistency rules a response is held to, run when the response is validated rather than
    /// on every read.
    ///
    /// A breakdown may *under*-report — a driver attributes only the modalities a provider names —
    /// but never exceed its total. Over-reporting is a contradiction, and it is caught here rather
    /// than being folded into a bill.
    pub fn validate(&self) -> Result<(), String> {
        validate_modalities("input", &self.details.input_modalities, self.input_tokens)?;
        validate_modalities(
            "output",
            &self.details.output_modalities,
            self.output_tokens,
        )?;
        if let Some(written) = self.input.cache_write_tokens
            && !self.details.cache_writes.is_empty()
        {
            let sum: u64 = self.details.cache_writes.values().sum();
            if sum != written {
                return Err(format!(
                    "cache write breakdown {sum} does not match cache write tokens {written}"
                ));
            }
        }
        if let Some(reasoning) = self.details.output.reasoning_tokens
            && self.details.output.reasoning_accounting == ReasoningAccounting::IncludedInOutput
            && reasoning > self.output_tokens
        {
            return Err("included reasoning tokens exceed output tokens".to_owned());
        }
        if let Some(billing) = &self.billing {
            billing.validate()?;
        }
        Ok(())
    }
}

impl Billing {
    fn validate(&self) -> Result<(), String> {
        if let Some(cost) = &self.cost {
            if cost.currency.is_empty() {
                return Err("billing currency is required".to_owned());
            }
            if cost.scale > 18 {
                return Err("billing cost scale must not exceed 18".to_owned());
            }
        }
        Ok(())
    }
}

fn validate_modalities(
    which: &str,
    usages: &BTreeMap<Modality, u64>,
    total: u64,
) -> Result<(), String> {
    let sum: u64 = usages.values().sum();
    if total > 0 && sum > total {
        return Err(format!(
            "{which} modality tokens total {sum} exceeds {which} tokens {total}"
        ));
    }
    Ok(())
}

/// Two empty maps stay empty, so "not reported" does not become a reported empty map.
fn merge_work(into: &mut BTreeMap<WorkKind, u64>, other: &BTreeMap<WorkKind, u64>) {
    for (kind, value) in other {
        *into.entry(*kind).or_insert(0) += value;
    }
}

/// Two `None`s stay `None`; otherwise the sum, saturating.
fn add_optional(into: &mut Option<u64>, other: Option<u64>) {
    if let Some(value) = other {
        *into = Some(into.unwrap_or(0).saturating_add(value));
    }
}

/// Fold one call's price into a running total.
///
/// `Usage::add_meter` deliberately does not do this: a price is not a meter, so it has no place in
/// the same fold as a token count. But a *ledger* is exactly where a total cost belongs, so it folds
/// the price explicitly, under three rules:
///
/// - **Both unreported stays unreported.** Two `None`s are `None`, never a reported zero.
/// - **One unreported keeps the other.** `None` means "this call did not say", not "this call cost
///   nothing", so it cannot erase a price another call reported.
/// - **Mismatched currency or scale is unknown.** `Money::add` refuses the conversion, and the
///   total becomes `None` rather than a number produced by guessing a rounding rule.
pub fn fold_billing(into: &mut Option<Billing>, other: &Option<Billing>) {
    // Adopt the first report outright: there is nothing to fold into yet.
    if into.is_none() {
        *into = other.clone();
        return;
    }
    let Some(total) = into.as_mut() else {
        return;
    };
    let Some(incoming) = other else {
        return;
    };
    total.cost = match (total.cost.take(), incoming.cost.clone()) {
        (Some(a), Some(b)) => a.add(&b),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    // The token halves of a bill are meters, not prices: they sum like any other counter.
    add_optional(&mut total.input_tokens, incoming.input_tokens);
    add_optional(&mut total.output_tokens, incoming.output_tokens);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn money(units: i64, scale: u8) -> Money {
        Money {
            currency: "USD".to_owned(),
            units,
            scale,
        }
    }

    #[test]
    fn money_add_sums_matching_currency_and_scale() {
        assert_eq!(money(100, 6).add(&money(50, 6)), Some(money(150, 6)));
    }

    #[test]
    fn money_add_refuses_a_different_scale_rather_than_guessing_a_conversion() {
        // Normalising is a policy decision about rounding, and this type does not make it.
        assert_eq!(money(100, 6).add(&money(50, 3)), None);
    }

    #[test]
    fn money_add_refuses_a_different_currency() {
        let other = Money {
            currency: "EUR".to_owned(),
            units: 50,
            scale: 6,
        };
        assert_eq!(money(100, 6).add(&other), None);
    }

    #[test]
    fn add_meter_sums_tokens_and_work() {
        let mut total = Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Usage::default()
        };
        total.add_work(WorkKind::ImagesGenerated, 2);
        let mut other = Usage {
            input_tokens: 3,
            output_tokens: 2,
            ..Usage::default()
        };
        other.add_work(WorkKind::ImagesGenerated, 1);
        other.add_work(WorkKind::AudioDurationMillis, 500);
        total.add_meter(&other);
        assert_eq!(total.input_tokens, 13);
        assert_eq!(total.output_tokens, 7);
        assert_eq!(total.work_value(WorkKind::ImagesGenerated), 3);
        assert_eq!(total.work_value(WorkKind::AudioDurationMillis), 500);
    }

    #[test]
    fn add_meter_does_not_touch_billing() {
        // A price is not a meter. Folding it in would invent a sum semantics no provider shares.
        let mut total = Usage {
            billing: Some(Billing {
                cost: Some(money(100, 6)),
                ..Billing::default()
            }),
            ..Usage::default()
        };
        total.add_meter(&Usage {
            billing: Some(Billing {
                cost: Some(money(50, 6)),
                ..Billing::default()
            }),
            ..Usage::default()
        });
        assert_eq!(total.billing.unwrap().cost, Some(money(100, 6)));
    }

    #[test]
    fn two_unreported_counters_stay_unreported() {
        // The rule the whole ledger depends on: `None` plus `None` is `None`, not `Some(0)`.
        let mut total = Usage::default();
        total.add_meter(&Usage::default());
        assert!(total.input.cache_read_tokens.is_none());
        assert!(total.work.is_empty());
    }

    #[test]
    fn an_unreported_counter_plus_a_report_is_the_report() {
        let mut total = Usage::default();
        total.add_meter(&Usage {
            input: InputTokenUsage {
                cache_read_tokens: Some(40),
                ..InputTokenUsage::default()
            },
            ..Usage::default()
        });
        assert_eq!(total.input.cache_read_tokens, Some(40));
    }

    #[test]
    fn a_report_plus_an_unreported_counter_keeps_the_report() {
        let mut total = Usage {
            input: InputTokenUsage {
                cache_read_tokens: Some(40),
                ..InputTokenUsage::default()
            },
            ..Usage::default()
        };
        total.add_meter(&Usage::default());
        assert_eq!(total.input.cache_read_tokens, Some(40));
    }

    #[test]
    fn a_breakdown_may_under_report_but_never_exceed_its_total() {
        let usage = Usage {
            input_tokens: 10,
            details: Details {
                input_modalities: BTreeMap::from([(Modality::Text, 4)]),
                ..Details::default()
            },
            ..Usage::default()
        };
        assert!(
            usage.validate().is_ok(),
            "under-reporting is unattributed, not an error"
        );

        let usage = Usage {
            input_tokens: 10,
            details: Details {
                input_modalities: BTreeMap::from([(Modality::Text, 40)]),
                ..Details::default()
            },
            ..Usage::default()
        };
        assert!(
            usage.validate().is_err(),
            "a breakdown larger than its total is a contradiction"
        );
    }

    #[test]
    fn a_cache_write_breakdown_must_match_its_total() {
        let usage = Usage {
            input: InputTokenUsage {
                cache_write_tokens: Some(10),
                ..InputTokenUsage::default()
            },
            details: Details {
                cache_writes: BTreeMap::from([(CacheTtl::FiveMinutes, 4)]),
                ..Details::default()
            },
            ..Usage::default()
        };
        assert!(usage.validate().is_err());
    }

    #[test]
    fn included_reasoning_cannot_exceed_the_output_total() {
        let usage = Usage {
            output_tokens: 5,
            details: Details {
                output: OutputTokenUsage {
                    reasoning_tokens: Some(10),
                    reasoning_accounting: ReasoningAccounting::IncludedInOutput,
                },
                ..Details::default()
            },
            ..Usage::default()
        };
        assert!(usage.validate().is_err());
    }

    #[test]
    fn a_cost_scale_beyond_eighteen_is_refused() {
        let usage = Usage {
            billing: Some(Billing {
                cost: Some(money(1, 19)),
                ..Billing::default()
            }),
            ..Usage::default()
        };
        assert!(usage.validate().is_err());
    }
}
