#![allow(clippy::unwrap_used)]

use super::*;
use crate::ToolName;
use serde::Deserialize;

#[test]
fn arguments_round_trip_as_an_object() {
    let call = ToolCall {
        id: "c1".to_owned(),
        name: ToolName::new("read").unwrap(),
        arguments: Arguments::new(r#"{"path":"/a"}"#),
    };
    let json = serde_json::to_string(&call).unwrap();
    assert!(json.contains(r#""arguments":{"path":"/a"}"#), "{json}");
    let back: ToolCall = serde_json::from_str(&json).unwrap();
    assert_eq!(back, call);
}

#[test]
fn arguments_decode_into_a_type() {
    #[derive(Deserialize)]
    struct Read {
        path: String,
    }
    let arguments = Arguments::new(r#"{"path":"/a"}"#);
    let read: Read = arguments.decode().unwrap();
    assert_eq!(read.path, "/a");
}

#[test]
fn an_absent_argument_object_is_empty_not_null() {
    let call: ToolCall = serde_json::from_str(r#"{"id":"c1","name":"ping"}"#).unwrap();
    assert_eq!(call.arguments.raw(), "{}");
}

#[test]
fn a_failed_result_still_carries_content() {
    let result = ToolResult::failed("c1", "no such file");
    assert!(result.is_error);
    assert_eq!(result.content.as_text(), "no such file");
}

#[test]
fn as_text_concatenates_only_text() {
    let content = Content::new([
        Part::Text {
            text: "hello ".to_owned(),
        },
        Part::Reasoning {
            text: "thinking".to_owned(),
            signature: None,
            id: None,
            source: None,
        },
        Part::Text {
            text: "world".to_owned(),
        },
    ]);
    assert_eq!(content.as_text(), "hello thinkingworld");
}

#[test]
fn a_message_round_trips_with_its_kind_tag() {
    let message = Message::user("hi");
    let json = serde_json::to_string(&message).unwrap();
    assert!(json.contains("\"kind\":\"text\""), "{json}");
    let back: Message = serde_json::from_str(&json).unwrap();
    assert_eq!(back, message);
}

#[test]
fn a_system_message_is_a_transcript_entry() {
    let message = Message::system("be terse");
    assert_eq!(message.role, Role::System);
    assert!(message.validate().is_ok());
}

#[test]
fn an_empty_message_is_refused() {
    let message = Message {
        role: Role::User,
        content: Content::default(),
        metadata: None,
    };
    assert!(
        message.validate().is_err(),
        "a provider answers this with a 400"
    );
}

#[test]
fn a_part_must_appear_under_the_role_that_can_say_it() {
    let call = Message {
        role: Role::User,
        content: Content::new([Part::ToolCall {
            call: crate::ToolCall {
                id: "c1".to_owned(),
                name: ToolName::new("read").unwrap(),
                arguments: crate::Arguments::empty(),
            },
        }]),
        metadata: None,
    };
    assert!(call.validate().is_err(), "a user cannot issue a tool call");

    let result = Message {
        role: Role::User,
        content: Content::new([Part::ToolResult {
            result: crate::ToolResult::ok("c1", Content::text("ok")),
        }]),
        metadata: None,
    };
    assert!(
        result.validate().is_err(),
        "a tool result needs the tool role"
    );
}

#[test]
fn a_tool_message_needs_a_result() {
    let message = Message {
        role: Role::Tool,
        content: Content::text("just prose"),
        metadata: None,
    };
    assert!(message.validate().is_err());
}

#[test]
fn metadata_round_trips_and_stays_out_of_the_payload_when_empty() {
    let plain = Message::user("hi");
    let json = serde_json::to_string(&plain).unwrap();
    assert!(!json.contains("metadata"), "{json}");

    let annotated = Message {
        metadata: Some(Metadata {
            reference_tasks: vec!["task-1".to_owned()],
            extensions: vec![],
            attributes: [("lang".to_owned(), serde_json::json!("zh"))]
                .into_iter()
                .collect(),
        }),
        ..Message::user("hi")
    };
    let json = serde_json::to_string(&annotated).unwrap();
    let back: Message = serde_json::from_str(&json).unwrap();
    assert_eq!(back, annotated);
}

fn call(id: &str, name: &str) -> Part {
    Part::ToolCall {
        call: crate::ToolCall {
            id: id.to_owned(),
            name: ToolName::new(name).unwrap(),
            arguments: crate::Arguments::empty(),
        },
    }
}

fn result(id: &str) -> Part {
    Part::ToolResult {
        result: crate::ToolResult::ok(id, Content::text("ok")),
    }
}

#[test]
fn a_conversation_whose_calls_are_answered_is_well_formed() {
    let messages = vec![
        Message::user("read /a"),
        Message {
            role: Role::Assistant,
            content: Content::new([call("c1", "read")]),
            metadata: None,
        },
        Message {
            role: Role::Tool,
            content: Content::new([result("c1")]),
            metadata: None,
        },
    ];
    assert!(validate_well_formed(&messages).is_ok());
}

#[test]
fn an_unanswered_call_is_refused_with_its_id() {
    let messages = vec![
        Message::user("read /a"),
        Message {
            role: Role::Assistant,
            content: Content::new([call("c1", "read")]),
            metadata: None,
        },
    ];
    let error = validate_well_formed(&messages).unwrap_err();
    assert!(error.message.contains("c1"), "{error:?}");
}

#[test]
fn a_result_for_a_call_that_never_happened_is_refused() {
    let messages = vec![
        Message::user("hi"),
        Message {
            role: Role::Tool,
            content: Content::new([result("ghost")]),
            metadata: None,
        },
    ];
    let error = validate_well_formed(&messages).unwrap_err();
    assert!(error.message.contains("ghost"), "{error:?}");
}

#[test]
fn a_user_turn_before_the_answers_is_refused() {
    let messages = vec![
        Message::user("read /a"),
        Message {
            role: Role::Assistant,
            content: Content::new([call("c1", "read")]),
            metadata: None,
        },
        Message::user("never mind"),
    ];
    assert!(validate_well_formed(&messages).is_err());
}

#[test]
fn a_second_assistant_turn_cannot_open_while_calls_are_pending() {
    let messages = vec![
        Message {
            role: Role::Assistant,
            content: Content::new([call("c1", "read")]),
            metadata: None,
        },
        Message {
            role: Role::Assistant,
            content: Content::new([call("c2", "read")]),
            metadata: None,
        },
    ];
    assert!(validate_well_formed(&messages).is_err());
}

#[test]
fn a_tool_the_session_cannot_use_is_refused_by_name() {
    let messages = vec![
        Message::user("do it"),
        Message {
            role: Role::Assistant,
            content: Content::new([call("c1", "shell")]),
            metadata: None,
        },
    ];
    let error = validate_callable(&messages, |name| name == "read").unwrap_err();
    assert!(error.message.contains("shell"), "{error:?}");
    assert!(validate_callable(&messages, |_| true).is_ok());
}

#[test]
fn every_kind_is_listed_exactly_once() {
    let mut seen = Vec::new();
    for kind in PartKind::all() {
        assert!(!seen.contains(kind), "{kind:?} listed twice");
        seen.push(*kind);
    }
    assert_eq!(seen.len(), 9);
}
