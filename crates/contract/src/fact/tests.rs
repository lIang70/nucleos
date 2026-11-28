#![allow(clippy::unwrap_used)]

use super::*;
use crate::content::Message;
use crate::identity::{AppId, SessionId, SessionKey};

fn key() -> SessionKey {
    SessionKey::new(
        AppId::new("writer").unwrap(),
        SessionId::new("draft-1").unwrap(),
    )
}

#[test]
fn positions_are_dense_and_refuse_to_wrap() {
    assert_eq!(Seq::FIRST.get(), 0);
    assert_eq!(Seq::FIRST.next(), Seq::new(1));
    let last = Seq::new(u64::MAX);
    assert!(last.is_exhausted());
    assert_eq!(
        last.next(),
        last,
        "wrapping would make a full log look empty"
    );
}

#[test]
fn a_blob_id_addresses_bytes_and_round_trips_as_hex() {
    let first = BlobId::of(b"the same bytes");
    assert_eq!(first, BlobId::of(b"the same bytes"));
    assert_ne!(first, BlobId::of(b"other bytes"));

    let hex = first.to_hex();
    assert_eq!(hex.len(), 64);
    assert_eq!(hex, hex.to_lowercase());
    assert_eq!(BlobId::from_hex(&hex).unwrap(), first);
    assert_eq!(BlobId::from_hex(&hex.to_uppercase()).unwrap(), first);
    assert!(BlobId::from_hex("abc").is_err());
    assert!(BlobId::from_hex(&"z".repeat(64)).is_err());

    assert_eq!(serde_json::to_string(&first).unwrap(), format!("\"{hex}\""));
    let decoded: BlobId = serde_json::from_str(&format!("\"{hex}\"")).unwrap();
    assert_eq!(decoded, first);
}

#[test]
fn equal_json_values_share_an_address_regardless_of_message_wrapping() {
    let message = Message::user("hello");
    let direct = BlobId::of_json(&message).unwrap();
    let round_tripped: Message =
        serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
    assert_eq!(direct, BlobId::of_json(&round_tripped).unwrap());
}

#[test]
fn a_fact_round_trips_with_its_position_and_session() {
    let fact = Fact::new(
        Seq::new(3),
        key(),
        1_700_000_000_000,
        FactKind::ModelCallStarted {
            model: crate::inference::ModelRef::new("openai/gpt-5").unwrap(),
        },
    );
    let json = serde_json::to_string(&fact).unwrap();
    assert!(json.contains(r#""kind":"model_call_started""#), "{json}");
    assert!(json.contains(r#""seq":3"#), "{json}");
    let decoded: Fact = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, fact);
}

#[test]
fn a_failed_call_can_still_carry_what_it_spent() {
    use crate::inference::{Outcome, Usage};

    let spent = Outcome {
        usage: Usage {
            input_tokens: 900,
            output_tokens: 12,
            ..Usage::default()
        },
        ..Outcome::default()
    };
    let fact = Fact::new(
        Seq::new(4),
        key(),
        1_700_000_000_000,
        FactKind::ModelCallFailed {
            error: crate::ErrorKind::Truncated,
            message: "the stream stopped early".to_owned(),
            outcome: Some(spent.clone()),
        },
    );
    let json = serde_json::to_string(&fact).unwrap();
    assert!(json.contains(r#""input_tokens":900"#), "{json}");
    let decoded: Fact = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, fact);
}

#[test]
fn an_unreported_failure_cost_is_absent_rather_than_zero() {
    let fact = Fact::new(
        Seq::new(5),
        key(),
        1_700_000_000_000,
        FactKind::ModelCallFailed {
            error: crate::ErrorKind::Unavailable,
            message: "nothing came back".to_owned(),
            outcome: None,
        },
    );
    let json = serde_json::to_value(&fact).unwrap();
    let kind = &json["kind"];
    assert!(
        kind.get("outcome").is_none(),
        "an unreported cost must not appear as a zero: {kind}"
    );
    let decoded: Fact = serde_json::from_value(json).unwrap();
    assert_eq!(decoded, fact);
}

#[test]
fn a_trace_can_be_joined_to_the_record() {
    let fact = Fact::new(
        Seq::new(0),
        key(),
        1_700_000_000_000,
        FactKind::SessionClosed {
            reason: CloseReason::Error,
        },
    )
    .traced("4bf92f3577b34da6a3ce929d0e0e4736");
    let json = serde_json::to_value(&fact).unwrap();
    assert_eq!(json["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(json["kind"]["reason"], "error");
    let decoded: Fact = serde_json::from_value(json).unwrap();
    assert_eq!(decoded, fact);
}
