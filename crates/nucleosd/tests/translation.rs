//! The wire translation, and the service's behaviour before a deployment exists.
//!
//! These are the tests that keep "the kernel does not learn the wire" true in both directions:
//! a value that survives a round trip proves the translation is total, and a value that does
//! not is a field somebody will discover in production.

#![allow(clippy::unwrap_used)]

use nucleos::contract as k;
use nucleos_proto::pb;
use nucleosd::translate;
use std::collections::BTreeMap;

fn message() -> k::Message {
    k::Message {
        role: k::Role::Assistant,
        content: k::Content::new([
            k::Part::new(k::PartKind::Text(k::TextPart {
                text: "hello".to_owned(),
            })),
            k::Part::new(k::PartKind::Reasoning(k::ReasoningPart {
                text: "thinking".to_owned(),
                signature: "sig".to_owned(),
                id: "rs_1".to_owned(),
                source: "anthropic".to_owned(),
            })),
            k::Part::new(k::PartKind::ToolCall(k::ToolCallPart {
                call: k::ToolCall {
                    id: "call-1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments_json: "{\"path\":\"a\"}".to_owned(),
                    meta: BTreeMap::from([("vendor".to_owned(), "x".to_owned())]),
                },
            })),
        ]),
    }
}

#[test]
fn a_message_survives_a_round_trip() {
    let original = message();
    let back = translate::message_from_pb(&translate::message_to_pb(&original));

    // Signature, id and source are opaque to the kernel and must come back verbatim: a provider
    // that rejects the next turn because its signature was dropped is a multi-turn session that
    // fails on turn two.
    assert_eq!(back, original);
}

#[test]
fn the_extension_hatch_survives_a_round_trip() {
    let mut part = k::Part::new(k::PartKind::Text(k::TextPart {
        text: "x".to_owned(),
    }));
    part.meta.insert("vendor.flag".to_owned(), "1".to_owned());

    let back = translate::message_from_pb(&pb::Message {
        role: pb::Role::User as i32,
        content: Some(pb::Content {
            parts: vec![
                translate::message_to_pb(&k::Message {
                    role: k::Role::User,
                    content: k::Content::new([part]),
                })
                .content
                .unwrap()
                .parts
                .remove(0),
            ],
        }),
    });

    assert_eq!(
        back.content.parts[0]
            .meta
            .get("vendor.flag")
            .map(String::as_str),
        Some("1")
    );
}

#[test]
fn an_unknown_part_kind_is_dropped_rather_than_failing_the_message() {
    // Additive is not the same as understood: a newer SDK's part kind must not break an older
    // reader, and must not silently turn into text either.
    let wire = pb::Message {
        role: pb::Role::User as i32,
        content: Some(pb::Content {
            parts: vec![pb::Part {
                meta: Default::default(),
                kind: None,
            }],
        }),
    };

    let back = translate::message_from_pb(&wire);
    assert!(back.content.parts.is_empty());
}

#[test]
fn an_event_carries_a_routable_type_and_a_translatable_kind() {
    let event = k::Event {
        seq: 7,
        session: k::SessionId::new("s1"),
        at_unix_millis: 1_700_000_000_123,
        schema_version: k::Event::SCHEMA_VERSION,
        kind: k::EventKind::TextDelta {
            part_index: 2,
            text: "hi".to_owned(),
        },
        meta: BTreeMap::from([("traceparent".to_owned(), "00-abc-def-01".to_owned())]),
    };

    let wire = translate::event(&event);

    assert_eq!(wire.seq, 7);
    assert_eq!(wire.r#type, "nucleos.v1.text_delta");
    assert_eq!(
        wire.meta.get("traceparent").map(String::as_str),
        Some("00-abc-def-01")
    );

    let at = wire.at.unwrap();
    assert_eq!(at.seconds, 1_700_000_000, "unix millis split into seconds");
    assert_eq!(at.nanos, 123_000_000, "and the remainder into nanos");

    match wire.kind {
        Some(pb::event::Kind::Text(delta)) => {
            assert_eq!(delta.part_index, 2);
            assert_eq!(delta.text, "hi");
        }
        other => panic!("expected a text delta, got {other:?}"),
    }
}

#[test]
fn a_tool_failure_keeps_its_flag_on_the_wire() {
    let result = k::ToolResult {
        call_id: "call-1".to_owned(),
        content: k::Content::new([k::Part::new(k::PartKind::Text(k::TextPart {
            text: "no such file".to_owned(),
        }))]),
        is_error: true,
    };
    let event = k::Event {
        seq: 0,
        session: k::SessionId::new("s1"),
        at_unix_millis: 0,
        schema_version: 1,
        kind: k::EventKind::ToolFinished {
            duration_millis: 4,
            result,
        },
        meta: BTreeMap::new(),
    };

    match translate::event(&event).kind {
        Some(pb::event::Kind::ToolResult(finished)) => {
            let result = finished.result.unwrap();
            assert!(
                result.is_error,
                "the flag is what tells the model it failed"
            );
            assert_eq!(result.content.unwrap().parts.len(), 1);
        }
        other => panic!("expected a tool result, got {other:?}"),
    }
}

#[test]
fn a_start_request_becomes_a_kernel_spec() {
    let request = pb::StartRequest {
        session: "s1".to_owned(),
        resume: false,
        spec: Some(pb::SessionSpec {
            model_chain: vec!["primary".to_owned(), "secondary".to_owned()],
            retries: 2,
            system: "be brief".to_owned(),
            initial: vec![translate::message_to_pb(&message())],
            tools: vec!["read_file".to_owned()],
            params: Some(pb::GenerateParams {
                max_output_tokens: Some(512),
                temperature: Some(0.2),
                top_p: None,
                stop: vec!["</s>".to_owned()],
                reasoning_effort: pb::ReasoningEffort::High as i32,
                tool_choice: pb::ToolChoice::Required as i32,
                format: Some(pb::TextFormat {
                    kind: pb::TextFormatKind::JsonSchema as i32,
                    schema_name: "answer".to_owned(),
                    schema_json: "{}".to_owned(),
                }),
            }),
            metadata: Default::default(),
        }),
        budget: Some(pb::Budget {
            max_steps: Some(8),
            max_tokens: None,
            max_micros_usd: Some(100),
            max_wall_millis: Some(30_000),
        }),
        credit_tokens: 4_096,
        client: None,
    };

    let spec = translate::spec_from_pb(&request);

    let routing = spec.routing.unwrap();
    assert_eq!(routing.chain, vec!["primary", "secondary"]);
    assert_eq!(routing.retries, 2);
    assert_eq!(spec.system.as_deref(), Some("be brief"));
    assert_eq!(spec.credit_tokens, 4_096);
    assert_eq!(spec.budget.max_steps, Some(8));
    assert_eq!(spec.params.max_output_tokens, Some(512));
    assert_eq!(
        spec.params.tool_choice,
        nucleos::adapter::ToolChoice::Required
    );
    assert!(matches!(
        spec.params.format,
        nucleos::adapter::TextFormat::JsonSchema { .. }
    ));
}

#[test]
fn an_empty_system_prompt_is_absent_rather_than_empty() {
    let request = pb::StartRequest {
        spec: Some(pb::SessionSpec::default()),
        ..pb::StartRequest::default()
    };

    // A wire message has no optional strings, so "" has to mean "absent" somewhere. It is
    // decided here, once, instead of in every adapter that reads it.
    assert!(translate::spec_from_pb(&request).system.is_none());
}

// ---------------------------------------------------------------------------
// The service before a deployment exists
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unconfigured_daemon_describes_itself_and_refuses_to_pretend() {
    use nucleos_proto::pb::kernel_server::Kernel as _;
    use tonic::Request;

    let service = nucleosd::KernelService::unconfigured();

    let described = service
        .describe(Request::new(pb::DescribeRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert!(described.models.is_empty(), "no deployment, no models");
    assert_eq!(
        described.abi_versions,
        vec![nucleos_proto::ABI_VERSION.to_owned()]
    );

    // Every method that needs a deployment says so in one sentence at the edge, instead of
    // failing three layers down with a message about a model nobody configured.
    let error = service
        .start(Request::new(pb::StartRequest::default()))
        .await
        .err()
        .unwrap();
    assert_eq!(error.code(), tonic::Code::Unavailable);
    assert!(error.message().contains("no deployment"));
}
