//! The shape of a stored event.
//!
//! `Event.kind` is flattened into the event, the way `Part.kind` already is. It used to nest —
//! `{"kind":{"kind":"text_delta",…}}` — because the field held a tagged enum without
//! `#[serde(flatten)]`, which put the tag twice in every frame and made the JSON tag sit one level
//! below the SSE event name derived from it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::contract::{Event, EventKind, SessionId};

fn event(kind: EventKind) -> Event {
    Event {
        seq: 7,
        session: SessionId::new("s1"),
        at_unix_millis: 0,
        schema_version: Event::SCHEMA_VERSION,
        kind,
        meta: std::collections::BTreeMap::new(),
    }
}

#[test]
fn the_tag_appears_once_and_at_the_top() {
    let json = serde_json::to_string(&event(EventKind::TextDelta {
        part_index: 0,
        text: "hi".to_owned(),
    }))
    .unwrap();
    assert!(json.contains(r#""kind":"text_delta""#), "{json}");
    assert!(
        !json.contains(r#""kind":{"kind""#),
        "the tag appears once: {json}"
    );
    assert!(
        json.contains(r#""part_index":0"#),
        "the fields are beside the tag: {json}"
    );
}

#[test]
fn every_kind_round_trips() {
    let kinds = [
        EventKind::TextDelta {
            part_index: 1,
            text: "hello".to_owned(),
        },
        EventKind::TurnStarted {
            model: "primary".to_owned(),
            generation: 3,
        },
        EventKind::BudgetExhausted {
            reason: nucleos::contract::StopReason::Tokens,
        },
    ];
    for kind in kinds {
        let original = event(kind);
        let json = serde_json::to_string(&original).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original, "{json}");
    }
}

#[test]
fn metadata_survives_beside_the_flattened_kind() {
    // Both are flattened-or-named on the same object, so this is the case where an extension key
    // could collide with a kind's field.
    let mut meta = std::collections::BTreeMap::new();
    meta.insert("trace".to_owned(), "abc".to_owned());
    let original = Event {
        meta,
        ..event(EventKind::BudgetExhausted {
            reason: nucleos::contract::StopReason::Tokens,
        })
    };
    let back: Event = serde_json::from_str(&serde_json::to_string(&original).unwrap()).unwrap();
    assert_eq!(back.meta.get("trace").map(String::as_str), Some("abc"));
    assert_eq!(back, original);
}
