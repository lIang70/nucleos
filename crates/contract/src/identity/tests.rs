#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn names_are_validated() {
    assert!(AppId::new("writer").is_ok());
    assert!(SessionId::new("draft-1").is_ok());
    for invalid in ["", "has space", "has/slash", "line\nbreak", "\0"] {
        assert!(AppId::new(invalid).is_err(), "accepted {invalid:?}");
        assert!(SessionId::new(invalid).is_err(), "accepted {invalid:?}");
    }
    assert!(AppId::new("a".repeat(MAX_NAME_LEN + 1)).is_err());
}

#[test]
fn names_preserve_case() {
    assert_ne!(AppId::new("Writer").unwrap(), AppId::new("writer").unwrap());
}

#[test]
fn the_same_session_name_is_isolated_by_application() {
    let session = SessionId::new("draft").unwrap();
    let left = SessionKey::new(AppId::new("left").unwrap(), session.clone());
    let right = SessionKey::new(AppId::new("right").unwrap(), session);
    assert_ne!(left, right);
}

#[test]
fn identity_round_trips_as_plain_json_strings() {
    let app = AppId::new("writer").unwrap();
    assert_eq!(serde_json::to_string(&app).unwrap(), "\"writer\"");
    let decoded: AppId = serde_json::from_str("\"writer\"").unwrap();
    assert_eq!(decoded, app);
    assert!(serde_json::from_str::<AppId>("\"bad/name\"").is_err());
    assert!(serde_json::from_str::<SessionId>("\"bad name\"").is_err());
}
