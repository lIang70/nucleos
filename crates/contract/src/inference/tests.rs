#![allow(clippy::unwrap_used)]

use super::*;

fn billed(input: u64, output: u64, micros: i64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        billing: Some(Billing {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cost: Some(Money::micro_usd(micros)),
        }),
        ..Usage::default()
    }
}

#[test]
fn total_tokens_is_input_plus_output() {
    let usage = Usage {
        input_tokens: 100,
        output_tokens: 50,
        ..Usage::default()
    };
    assert_eq!(usage.total_tokens(), 150);
}

#[test]
fn add_meter_sums_meters_and_drops_the_price() {
    let total = billed(100, 50, 10).add_meter(&billed(20, 5, 3));
    assert_eq!(total.input_tokens, 120);
    assert_eq!(total.output_tokens, 55);
    assert!(
        total.billing.is_none(),
        "a price is not a meter, and add_meter must not invent one"
    );
}

#[test]
fn fold_keeps_an_unreported_price_unreported() {
    let total = Usage::default().fold(&billed(20, 5, 3));
    assert!(
        total.billing.is_none(),
        "one reported half does not make an unknown whole known"
    );
}

#[test]
fn fold_adds_two_prices_in_the_same_money() {
    let total = billed(10, 1, 7).fold(&billed(10, 1, 5));
    assert_eq!(
        total.billing.and_then(|billing| billing.cost),
        Some(Money::micro_usd(12))
    );
}

#[test]
fn a_price_in_another_currency_or_scale_is_refused_rather_than_converted() {
    let usd = Money::micro_usd(100);
    let eur = Money {
        currency: "EUR".to_owned(),
        units: 100,
        scale: 6,
    };
    assert!(usd.checked_add(&eur).is_none());
    let coarse = Money {
        currency: "USD".to_owned(),
        units: 1,
        scale: 2,
    };
    assert!(
        usd.checked_add(&coarse).is_none(),
        "a different scale is a different unit"
    );
}

#[test]
fn a_meter_of_zero_is_reported_and_an_absent_one_is_not() {
    let mut usage = Usage::default();
    usage.work.insert(WorkKind::ImagesGenerated, 0);
    assert_eq!(usage.work_value(WorkKind::ImagesGenerated), 0);
    assert!(usage.work.contains_key(&WorkKind::ImagesGenerated));
    assert!(!usage.work.contains_key(&WorkKind::VideosGenerated));
}

#[test]
fn add_meter_keeps_one_unknown_bucket_unknown() {
    let left = Usage {
        input_tokens: 10,
        input: InputTokenUsage {
            cache_read_tokens: Some(4),
            ..InputTokenUsage::default()
        },
        ..Usage::default()
    };
    let right = Usage {
        input_tokens: 10,
        input: InputTokenUsage {
            cache_read_tokens: None,
            ..InputTokenUsage::default()
        },
        ..Usage::default()
    };
    let total = left.add_meter(&right);
    assert_eq!(
        total.input.cache_read_tokens, None,
        "an unknown bucket must not become zero"
    );
    assert_eq!(total.input_tokens, 20);
}

#[test]
fn cache_buckets_may_not_exceed_the_prompt_total() {
    let usage = Usage {
        input_tokens: 5,
        input: InputTokenUsage {
            cache_read_tokens: Some(4),
            cache_write_tokens: Some(4),
            uncached_tokens: None,
        },
        ..Usage::default()
    };
    assert!(usage.validate().is_err());
}

#[test]
fn one_reported_cache_bucket_is_still_checked_against_the_total() {
    let usage = Usage {
        input_tokens: 1,
        input: InputTokenUsage {
            cache_read_tokens: Some(10),
            cache_write_tokens: None,
            uncached_tokens: None,
        },
        ..Usage::default()
    };
    assert!(
        usage.validate().is_err(),
        "an unreported sibling must not hide a bucket that exceeds the total"
    );
}

#[test]
fn a_breakdown_may_not_exceed_its_total() {
    let usage = Usage {
        input_tokens: 10,
        details: Details {
            input_modalities: [(Modality::Image, 11)].into_iter().collect(),
            ..Details::default()
        },
        ..Usage::default()
    };
    assert!(usage.validate().is_err());
}

#[test]
fn reasoning_inside_the_output_total_cannot_exceed_it() {
    let usage = Usage {
        output_tokens: 10,
        details: Details {
            output: OutputTokenUsage {
                reasoning_tokens: Some(11),
                reasoning_accounting: ReasoningAccounting::IncludedInOutput,
            },
            ..Details::default()
        },
        ..Usage::default()
    };
    assert!(usage.validate().is_err());

    let additional = Usage {
        output_tokens: 10,
        details: Details {
            output: OutputTokenUsage {
                reasoning_tokens: Some(11),
                reasoning_accounting: ReasoningAccounting::Additional,
            },
            ..Details::default()
        },
        ..Usage::default()
    };
    assert!(
        additional.validate().is_ok(),
        "reasoning accounted separately may exceed the output total"
    );
}

#[test]
fn an_outcome_carries_both_halves() {
    let outcome = Outcome {
        metadata: Metadata {
            request_id: None,
            response_id: Some("resp_1".to_owned()),
        },
        usage: billed(1, 2, 3),
    };
    let json = serde_json::to_string(&outcome).unwrap();
    let back: Outcome = serde_json::from_str(&json).unwrap();
    assert_eq!(back, outcome);
}
