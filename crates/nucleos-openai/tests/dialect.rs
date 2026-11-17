//! The Responses dialect, decoded and encoded without a network.
//!
//! Every payload below was captured from the live endpoint. That is the point of this file: the
//! event names, the flat tool shape and `call_id`-not-`id` are things a document skim gets wrong.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::adapter::{
    FinishReason, ModelDelta, ModelRequest, ReasoningEffort, TextFormat, ToolChoice,
};
use nucleos::contract::Hints;
use nucleos::contract::{
    Content, Message, Part, PartKind, Role, TextPart, ToolCall, ToolCallPart, ToolResult,
    ToolResultPart, ToolSpec,
};
use nucleos_openai::decode::Decoder;
use nucleos_openai::wire::{Encoded, Framer, Settings, encode};
use serde_json::json;

#[test]
fn text_arrives_as_a_delta() {
    let mut decoder = Decoder::default();
    let deltas = decoder
        .push(
            r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"Hello","sequence_number":3}"#,
        )
        .unwrap();

    assert_eq!(
        deltas,
        vec![ModelDelta::PartAppend {
            part_index: 0,
            part: nucleos::contract::Part::text("Hello".to_owned())
        }]
    );
}

#[test]
fn a_tool_call_is_announced_by_its_item_and_keyed_by_call_id() {
    // Captured shape: the item carries both `id` (provider-internal) and `call_id` (what a
    // `function_call_output` must reference). Only the second one is useful to a caller.
    let mut decoder = Decoder::default();
    let deltas = decoder
        .push(
            r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"fc_abc","type":"function_call","status":"in_progress","arguments":"","call_id":"call_xyz","name":"read_file"},"sequence_number":2}"#,
        )
        .unwrap();

    assert_eq!(
        deltas,
        vec![ModelDelta::ToolCallStarted {
            index: 0,
            part_index: 0,
            id: "call_xyz".to_owned(),
            name: "read_file".to_owned()
        }]
    );
}

#[test]
fn the_same_item_is_not_announced_twice() {
    let payload = r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_xyz","name":"read_file"}}"#;
    let mut decoder = Decoder::default();
    assert_eq!(decoder.push(payload).unwrap().len(), 1);
    assert!(decoder.push(payload).unwrap().is_empty());
}

#[test]
fn arguments_stream_as_fragments() {
    let mut decoder = Decoder::default();
    decoder
        .push(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"c1","name":"read_file"}}"#)
        .unwrap();

    let deltas = decoder
        .push(
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_abc","output_index":0,"delta":"{\"path\":\"/etc/hosts\"}","sequence_number":4}"#,
        )
        .unwrap();

    assert_eq!(
        deltas,
        vec![ModelDelta::ToolCallArgs {
            index: 0,
            fragment: "{\"path\":\"/etc/hosts\"}".to_owned()
        }]
    );
}

#[test]
fn completion_reports_usage_and_says_a_tool_was_called() {
    let mut decoder = Decoder::default();
    let deltas = decoder
        .push(
            r#"{"type":"response.completed","sequence_number":12,"response":{"status":"completed","output":[{"id":"fc_abc","type":"function_call","status":"completed","arguments":"{\"path\":\"/etc/hosts\"}","call_id":"call_xyz","name":"read_file"}],"usage":{"input_tokens":53,"input_tokens_details":{"cached_tokens":0},"output_tokens":9,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":62}}}"#,
        )
        .unwrap();

    match &deltas[0] {
        ModelDelta::Usage(usage) => {
            assert_eq!(usage.input_tokens, 53);
            assert_eq!(usage.output_tokens, 9);
            assert_eq!(
                usage.micros_usd, None,
                "the API reports tokens, never money"
            );
        }
        other => panic!("expected usage, got {other:?}"),
    }
    assert_eq!(
        deltas[1],
        ModelDelta::Finished {
            reason: FinishReason::ToolCalls
        }
    );
    assert_eq!(decoder.finish(), Some(&FinishReason::ToolCalls));
}

#[test]
fn an_answer_without_a_tool_call_finishes_with_stop() {
    let mut decoder = Decoder::default();
    let deltas = decoder
        .push(
            r#"{"type":"response.completed","response":{"status":"completed","output":[{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}],"usage":{"input_tokens":3,"output_tokens":1}}}"#,
        )
        .unwrap();
    assert_eq!(
        deltas.last(),
        Some(&ModelDelta::Finished {
            reason: FinishReason::Stop
        })
    );
}

#[test]
fn a_cut_off_answer_says_length_not_stop() {
    let mut decoder = Decoder::default();
    let deltas = decoder
        .push(r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}}"#)
        .unwrap();
    assert_eq!(
        deltas,
        vec![ModelDelta::Finished {
            reason: FinishReason::Length
        }]
    );
}

#[test]
fn a_failed_response_is_an_error_not_a_finish() {
    let mut decoder = Decoder::default();
    let error = decoder
        .push(r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"boom"}}}"#)
        .unwrap_err();
    assert_eq!(error.code(), nucleos::contract::ErrorCode::Unavailable);
    assert!(error.reason().contains("boom"));
}

#[test]
fn a_streamed_failure_lands_in_the_right_category() {
    // Captured from the live endpoint for an unsupported `service_tier`: an HTTP 200 that then
    // fails the response. `invalid_request_error` is the caller's mistake, not an outage.
    let mut decoder = Decoder::default();
    let error = decoder
        .push(r#"{"type":"error","error":{"type":"invalid_request_error","code":null,"message":"There was an issue with your request.","param":null},"sequence_number":2}"#)
        .unwrap_err();
    assert_eq!(error.code(), nucleos::contract::ErrorCode::Invalid);
    assert!(error.reason().contains("issue with your request"));

    let mut decoder = Decoder::default();
    let error = decoder
        .push(r#"{"type":"response.failed","response":{"status":"failed","error":{"type":"server_error","code":"unknown","message":"boom"}}}"#)
        .unwrap_err();
    assert_eq!(error.code(), nucleos::contract::ErrorCode::Unavailable);
}

#[test]
fn events_that_say_nothing_useful_produce_nothing() {
    let mut decoder = Decoder::default();
    for payload in [
        r#"{"type":"response.created","response":{"status":"in_progress"}}"#,
        r#"{"type":"response.in_progress"}"#,
        r#"{"type":"response.content_part.added","output_index":0}"#,
        r#"{"type":"response.output_text.done","text":"hi"}"#,
        "",
        "[DONE]",
    ] {
        assert!(
            decoder.push(payload).unwrap().is_empty(),
            "{payload} produced deltas"
        );
    }
}

#[test]
fn reasoning_summaries_become_reasoning_deltas() {
    let mut decoder = Decoder::default();
    let deltas = decoder
        .push(r#"{"type":"response.reasoning_summary_text.delta","output_index":1,"delta":"thinking"}"#)
        .unwrap();
    assert_eq!(
        deltas,
        vec![ModelDelta::PartAppend {
            part_index: 1,
            part: nucleos::contract::Part::reasoning("thinking".to_owned(), String::new())
        }]
    );
}

// ---------------------------------------------------------------------------
// Request encoding
// ---------------------------------------------------------------------------

/// Encode with the defaults, which is what a caller who configured nothing gets.
fn encoded(request: &ModelRequest) -> Encoded {
    encode(
        request,
        Settings {
            model: "gpt-x",
            supports_reasoning: false,
            strict_tools: false,
            options: &Hints::default(),
            cache_key: None,
            include: &[],
            safety_identifier: None,
        },
    )
    .expect("a text request encodes")
}

fn request() -> ModelRequest {
    ModelRequest {
        system: Some("Be terse.".to_owned()),
        messages: vec![Message {
            role: Role::User,
            content: Content::new([Part::new(PartKind::Text(TextPart {
                text: "hi".to_owned(),
            }))]),
        }]
        .into(),
        ..ModelRequest::default()
    }
}

#[test]
fn the_request_is_always_stateless() {
    // The provider can keep the conversation for us. It must not: the kernel's event log is the
    // record, and a provider holding half the state makes replay a reconstruction.
    let encoded = encoded(&request());
    assert_eq!(encoded.body["store"], false);
    assert_eq!(encoded.body["stream"], true);
    assert!(encoded.body.get("previous_response_id").is_none());
}

#[test]
fn a_system_prompt_becomes_instructions() {
    let encoded = encoded(&request());
    assert_eq!(encoded.body["instructions"], "Be terse.");
}

#[test]
fn a_tool_is_flat_and_strict_only_when_asked() {
    let request = ModelRequest {
        tools: vec![ToolSpec {
            name: "read_file".to_owned(),
            description: "reads".to_owned(),
            input_schema_json: r#"{"type":"object"}"#.to_owned(),
            isolation: Some(nucleos::contract::Isolation::InProcess),
        }],
        ..request()
    };
    let lenient = encoded(&request);
    let tool = &lenient.body["tools"][0];
    assert_eq!(tool["type"], "function");
    assert_eq!(
        tool["name"], "read_file",
        "flat, not nested under `function`"
    );
    assert_eq!(tool["parameters"]["type"], "object");
    // Off by default: the provider rejects `strict: true` for a schema without
    // `additionalProperties: false`, and it rejects the whole call with it.
    assert!(tool.get("strict").is_none());

    let strict = encode(
        &request,
        Settings {
            model: "gpt-x",
            supports_reasoning: false,
            strict_tools: true,
            options: &Hints::default(),
            cache_key: None,
            include: &[],
            safety_identifier: None,
        },
    )
    .expect("this request encodes");
    assert_eq!(strict.body["tools"][0]["strict"], true);
}

#[test]
fn history_tool_calls_are_input_items() {
    let request = ModelRequest {
        messages: vec![
            Message {
                role: Role::Assistant,
                content: Content::new([Part::new(PartKind::ToolCall(ToolCallPart {
                    call: ToolCall {
                        id: "call_xyz".to_owned(),
                        name: "read_file".to_owned(),
                        arguments_json: "{}".to_owned(),
                        meta: Default::default(),
                    },
                }))]),
            },
            Message {
                role: Role::Tool,
                content: Content::new([Part::new(PartKind::ToolResult(ToolResultPart {
                    result: ToolResult {
                        call_id: "call_xyz".to_owned(),
                        content: Content::new([Part::new(PartKind::Text(TextPart {
                            text: "127.0.0.1 localhost".to_owned(),
                        }))]),
                        is_error: false,
                    },
                }))]),
            },
        ]
        .into(),
        ..ModelRequest::default()
    };

    let encoded = encoded(&request);
    let input = encoded.body["input"].as_array().unwrap();
    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["type"], "function_call");
    assert_eq!(input[0]["call_id"], "call_xyz");
    assert_eq!(input[1]["type"], "function_call_output");
    assert_eq!(input[1]["call_id"], "call_xyz");
    assert_eq!(input[1]["output"], "127.0.0.1 localhost");
}

#[test]
fn stop_is_reported_rather_than_dropped_in_silence() {
    // Verified against the live endpoint: `Unknown parameter: 'stop'`. Sending it is a 400 that
    // kills the call; ignoring it silently hides why the text kept going.
    let request = ModelRequest {
        params: nucleos::adapter::GenerateParams {
            stop: vec!["\n".to_owned()],
            ..Default::default()
        },
        ..request()
    };
    let encoded = encoded(&request);

    assert!(encoded.body.get("stop").is_none());
    assert_eq!(encoded.dropped.len(), 1);
    assert_eq!(
        encoded.dropped[0].field,
        nucleos::contract::Field::Param(nucleos::contract::Param::Stop)
    );
}

#[test]
fn reasoning_effort_is_sent_only_where_it_is_supported() {
    let params = nucleos::adapter::GenerateParams {
        reasoning_effort: Some(ReasoningEffort::High),
        ..Default::default()
    };

    let supported = encode(
        &ModelRequest {
            params: params.clone(),
            ..request()
        },
        Settings {
            model: "gpt-x",
            supports_reasoning: true,
            strict_tools: false,
            options: &Hints::default(),
            cache_key: None,
            include: &[],
            safety_identifier: None,
        },
    )
    .expect("this request encodes");
    assert_eq!(supported.body["reasoning"]["effort"], "high");
    assert!(supported.dropped.is_empty());

    // gpt-4o-mini answers `unsupported_parameter` to this, and a 400 over a hint is worse than
    // saying so.
    let unsupported = encode(
        &ModelRequest {
            params,
            ..request()
        },
        Settings {
            model: "gpt-x",
            supports_reasoning: false,
            strict_tools: false,
            options: &Hints::default(),
            cache_key: None,
            include: &[],
            safety_identifier: None,
        },
    )
    .expect("this request encodes");
    assert!(unsupported.body.get("reasoning").is_none());
    assert_eq!(
        unsupported.dropped[0].field,
        nucleos::contract::Field::Param(nucleos::contract::Param::ReasoningEffort)
    );
}

#[test]
fn json_schema_becomes_a_strict_text_format() {
    let request = ModelRequest {
        params: nucleos::adapter::GenerateParams {
            format: TextFormat::JsonSchema {
                name: "answer".to_owned(),
                schema_json: r#"{"type":"object"}"#.to_owned(),
            },
            ..Default::default()
        },
        ..request()
    };
    let encoded = encoded(&request);

    let format = &encoded.body["text"]["format"];
    assert_eq!(format["type"], "json_schema");
    assert_eq!(format["name"], "answer");
    assert_eq!(format["strict"], true);
}

#[test]
fn parallel_tool_calls_reaches_the_wire() {
    // In the contract because the kernel acts on it: it fans a batch out concurrently, and a
    // provider that forbids it would disagree about what happened in a turn.
    let request = ModelRequest {
        params: nucleos::adapter::GenerateParams {
            parallel_tool_calls: Some(false),
            tool_choice: ToolChoice::Required,
            ..Default::default()
        },
        ..request()
    };
    let encoded = encoded(&request);
    assert_eq!(encoded.body["parallel_tool_calls"], false);
    assert_eq!(encoded.body["tool_choice"], "required");
}

#[test]
fn an_image_is_an_input_image() {
    let request = ModelRequest {
        messages: vec![Message {
            role: Role::User,
            content: Content::new([Part::image(
                nucleos::contract::ImageSource::url("https://example.test/x.png", "image/png")
                    .unwrap(),
            )]),
        }]
        .into(),
        ..ModelRequest::default()
    };
    let encoded = encoded(&request);
    let content = &encoded.body["input"][0]["content"][0];
    assert_eq!(content["type"], "input_image");
    assert_eq!(content["image_url"], "https://example.test/x.png");
}

#[test]
fn the_framer_survives_a_boundary_mid_event_and_ignores_metadata_lines() {
    let mut framer = Framer::default();
    assert!(
        framer
            .push(b"event: response.created\nid: 1\ndata: {\"a\":")
            .is_empty()
    );
    let payloads = framer.push(b"1}\n\n");
    assert_eq!(payloads, vec!["{\"a\":1}".to_owned()]);
}

#[test]
fn the_encoded_body_is_json_serialisable() {
    // A silent NaN or a non-string map key would only surface at runtime otherwise.
    let encoded = encoded(&request());
    let rendered = serde_json::to_string(&encoded.body).unwrap();
    assert!(rendered.contains("\"model\":\"gpt-x\""));
    assert!(encoded.body.is_object());
    let _ = json!(encoded.dropped);
}

// ---------------------------------------------------------------------------
// The deployment's parameters
// ---------------------------------------------------------------------------

fn settings(options: &Hints) -> Settings<'_> {
    Settings {
        model: "gpt-x",
        supports_reasoning: false,
        strict_tools: false,
        options,
        cache_key: None,
        include: &[],
        safety_identifier: None,
    }
}

#[test]
fn every_deployment_parameter_reaches_the_wire() {
    use nucleos_openai::wire::{Include, ServiceTier, Summary, Truncation, Verbosity};

    let options = Hints {
        metadata: [("tenant".to_owned(), "acme".to_owned())].into(),
        service_tier: Some(ServiceTier::Flex),
        truncation: Some(Truncation::Disabled),
        max_tool_calls: Some(4),
        verbosity: Some(Verbosity::Low),
        reasoning_summary: Some(Summary::Concise),
    };
    // The cache key, the extra data and the safety identifier are deployment facts, so they ride
    // on the settings rather than on the session.
    let encoded = encode(
        &request(),
        Settings {
            cache_key: Some("cache-1"),
            include: &[Include::ReasoningEncryptedContent],
            safety_identifier: Some("user-hash"),
            ..settings(&options)
        },
    )
    .expect("this request encodes");

    assert_eq!(encoded.body["metadata"]["tenant"], "acme");
    assert_eq!(encoded.body["service_tier"], "flex");
    assert_eq!(encoded.body["prompt_cache_key"], "cache-1");
    assert_eq!(encoded.body["truncation"], "disabled");
    assert_eq!(encoded.body["max_tool_calls"], 4);
    assert_eq!(encoded.body["include"][0], "reasoning.encrypted_content");
    assert_eq!(encoded.body["text"]["verbosity"], "low");
    assert_eq!(encoded.body["reasoning"]["summary"], "concise");
    assert_eq!(encoded.body["safety_identifier"], "user-hash");
}

#[test]
fn a_deployment_parameter_merges_with_a_call_parameter() {
    use nucleos_openai::wire::Verbosity;

    // A call that asked for a JSON schema and a deployment that asked for verbosity should get
    // both, in one `text` object. Overwriting either would silently drop the other.
    let request = ModelRequest {
        params: nucleos::adapter::GenerateParams {
            format: TextFormat::JsonSchema {
                name: "answer".to_owned(),
                schema_json: r#"{"type":"object"}"#.to_owned(),
            },
            ..Default::default()
        },
        ..request()
    };
    let options = Hints {
        verbosity: Some(Verbosity::High),
        ..Default::default()
    };
    let encoded = encode(&request, settings(&options)).expect("a text request encodes");

    assert_eq!(encoded.body["text"]["format"]["type"], "json_schema");
    assert_eq!(encoded.body["text"]["verbosity"], "high");
}

#[test]
fn nothing_is_sent_when_nothing_was_configured() {
    let encoded = encode(&request(), settings(&Hints::default())).expect("a text request encodes");
    for sent in [
        "metadata",
        "service_tier",
        "prompt_cache_key",
        "truncation",
        "max_tool_calls",
        "include",
        "text",
        "reasoning",
        "safety_identifier",
    ] {
        assert!(encoded.body.get(sent).is_none(), "{sent} was sent unasked");
    }
}

/// A payload the decoder cannot read is a broken answer, not an unreachable provider.
///
/// It was filed as `Unavailable` — which is retryable — so a wire format this adapter does not
/// understand was re-sent, at cost, into the same decoder. Nothing about a retry can make a
/// truncated JSON frame parse.
#[test]
fn an_undecodable_payload_is_not_retried() {
    let mut decoder = Decoder::default();
    let error = decoder
        .push(r#"{"type":"response.output_text.delta","delta":"#)
        .unwrap_err();

    assert_eq!(
        error.code(),
        nucleos::contract::ErrorCode::InvalidProviderResponse
    );
    assert!(
        !error.code().is_retryable(),
        "an undecodable payload was marked retryable: {}",
        error.reason()
    );
}

/// A forced call is encoded as the Responses API's named-function choice, not a bare string.
#[test]
fn a_forced_tool_is_encoded_as_a_named_function() {
    let body = encode(
        &nucleos::adapter::ModelRequest {
            tools: vec![ToolSpec {
                name: "read_file".to_owned(),
                description: "Read a file".to_owned(),
                input_schema_json: "{}".to_owned(),
                isolation: None,
            }],
            params: nucleos::adapter::GenerateParams {
                tool_choice: nucleos::contract::ToolChoice::Function {
                    name: "read_file".to_owned(),
                },
                ..nucleos::adapter::GenerateParams::default()
            },
            ..nucleos::adapter::ModelRequest::default()
        },
        Settings {
            model: "gpt-test",
            supports_reasoning: false,
            strict_tools: false,
            options: &nucleos::contract::Hints::default(),
            cache_key: None,
            include: &[],
            safety_identifier: None,
        },
    )
    .unwrap()
    .body;

    assert_eq!(
        body.get("tool_choice"),
        Some(&serde_json::json!({"type": "function", "name": "read_file"})),
        "{body}"
    );
}

// ---------------------------------------------------------------------------
// The image surface
// ---------------------------------------------------------------------------

/// An image request encodes the prompt and the options, and reports every field the endpoint has no
/// place for rather than leaving it silently absent.
#[test]
fn an_image_request_encodes_a_prompt_and_explains_the_rest() {
    use nucleos::adapter::{ExtensionEntry, Model, decode_extensions};
    use nucleos::contract::Output;
    use nucleos_openai::extension::IMAGE_DECODER;

    let mut registry = std::collections::BTreeMap::new();
    registry.insert(
        "openai/image".to_owned(),
        IMAGE_DECODER as nucleos::adapter::ExtensionDecoder,
    );
    let extensions = decode_extensions(
        &[ExtensionEntry {
            provider: "openai".to_owned(),
            id: "image".to_owned(),
            fields: json!({
                "size": "1024x1024",
                "quality": "high",
                "output_format": "webp",
                "partial_images": 2,
            }),
        }],
        &registry,
        "inference.extensions",
    )
    .unwrap();

    let request = ModelRequest {
        output: Output::Image,
        messages: vec![Message {
            role: Role::User,
            content: Content::new([Part::text("a red fox".to_owned())]),
        }]
        .into(),
        params: nucleos::adapter::GenerateParams {
            temperature: Some(0.5),
            ..Default::default()
        },
        extensions,
        ..ModelRequest::default()
    };

    let adapter = nucleos_openai::OpenAi::new(
        nucleos_openai::Config {
            url: "https://example.test/v1/responses".to_owned(),
            api_key: None,
            model: "gpt-image-1".to_owned(),
            timeout: None,
            images_url: Some("https://example.test/v1/images/generations".to_owned()),
            speech_url: None,
        },
        nucleos_openai::OpenAiSettings::default(),
    )
    .unwrap();
    let prepared = adapter.prepare(request).unwrap();
    let encoded = prepared
        .artifact::<nucleos_openai::wire::Encoded>()
        .unwrap();

    assert_eq!(encoded.endpoint, nucleos_openai::wire::Endpoint::Images);
    assert_eq!(encoded.body["prompt"], "a red fox");
    assert_eq!(encoded.body["size"], "1024x1024");
    assert_eq!(encoded.body["quality"], "high");
    assert_eq!(encoded.body["output_format"], "webp");
    assert_eq!(encoded.body["partial_images"], 2);
    assert_eq!(encoded.body["stream"], true);
    assert_eq!(
        encoded.answer_media_type.as_deref(),
        Some("image/webp"),
        "the part's media type must be the format that was asked for"
    );

    // Temperature has no place in an image body, and silence about it would be the defect.
    let dropped: Vec<String> = prepared
        .plan()
        .degraded
        .iter()
        .map(|entry| entry.field.as_str())
        .collect();
    assert!(
        dropped.iter().any(|field| field == "param.temperature"),
        "an unhonoured knob was not reported: {dropped:?}"
    );
}

/// A preview replaces, and the finished image replaces it — one part index, two frames.
#[test]
fn image_previews_and_the_finished_image_share_one_part() {
    use nucleos::contract::PartKind;
    use nucleos_openai::decode::ImageDecoder;

    let mut decoder = ImageDecoder::new("image/png");
    let preview = decoder
        .push(
            r#"{"type":"image_generation.partial_image","b64_json":"cHJldmlldw==","partial_image_index":0,"size":"1024x1024","quality":"low","background":"auto","output_format":"png"}"#,
        )
        .unwrap();
    assert_eq!(preview.len(), 1);
    assert!(matches!(
        &preview[0],
        ModelDelta::PartReplace { part_index: 0, part }
            if matches!(&part.kind, PartKind::Image(image) if image.source.bytes() == Some(b"preview"))
    ));

    let final_frame = decoder
        .push(
            r#"{"type":"image_generation.completed","b64_json":"ZmluYWw=","created_at":0,"size":"1024x1024","quality":"low","background":"auto","output_format":"png","usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}}"#,
        )
        .unwrap();
    assert!(matches!(
        &final_frame[0],
        ModelDelta::PartReplace { part_index: 0, part }
            if matches!(&part.kind, PartKind::Image(image) if image.source.bytes() == Some(b"final"))
    ));
    assert!(matches!(
        &final_frame[1],
        ModelDelta::Usage(usage) if usage.input_tokens == 2 && usage.output_tokens == 3
    ));
    assert!(matches!(&final_frame[2], ModelDelta::Finished { .. }));
    assert!(decoder.done());
}

/// A payload that is not base64 is an unreadable answer, not an unreachable provider.
#[test]
fn an_undecodable_image_payload_is_not_retried() {
    use nucleos_openai::decode::ImageDecoder;

    let mut decoder = ImageDecoder::new("image/png");
    let error = decoder
        .push(r#"{"type":"image_generation.partial_image","b64_json":"not base64!!"}"#)
        .unwrap_err();
    assert_eq!(
        error.code(),
        nucleos::contract::ErrorCode::InvalidProviderResponse
    );
}

/// A model with no image endpoint refuses an image request before anything is sent.
#[test]
fn an_image_request_without_an_endpoint_is_refused_before_the_call() {
    use nucleos::adapter::Model;
    use nucleos::contract::Output;

    let adapter = nucleos_openai::OpenAi::new(
        nucleos_openai::Config {
            url: "https://example.test/v1/responses".to_owned(),
            api_key: None,
            model: "gpt-4.1".to_owned(),
            timeout: None,
            images_url: None,
            speech_url: None,
        },
        nucleos_openai::OpenAiSettings::default(),
    )
    .unwrap();
    let request = ModelRequest {
        output: Output::Image,
        messages: vec![Message {
            role: Role::User,
            content: Content::new([Part::text("a red fox".to_owned())]),
        }]
        .into(),
        ..ModelRequest::default()
    };
    let error = adapter.prepare(request).unwrap_err();
    assert_eq!(
        error.code(),
        nucleos::contract::ErrorCode::UnsupportedFeature
    );
    assert!(error.reason().contains("no images"), "{error}");
}

/// An image call with no prompt is a request the endpoint cannot express.
#[test]
fn an_image_request_without_a_prompt_is_refused() {
    use nucleos::adapter::Model;
    use nucleos::contract::Output;

    let adapter = nucleos_openai::OpenAi::new(
        nucleos_openai::Config {
            url: "https://example.test/v1/responses".to_owned(),
            api_key: None,
            model: "gpt-image-1".to_owned(),
            timeout: None,
            images_url: Some("https://example.test/v1/images/generations".to_owned()),
            speech_url: None,
        },
        nucleos_openai::OpenAiSettings::default(),
    )
    .unwrap();
    let error = adapter
        .prepare(ModelRequest {
            output: Output::Image,
            messages: vec![Message {
                role: Role::User,
                content: Content::new([Part::text("   ".to_owned())]),
            }]
            .into(),
            ..ModelRequest::default()
        })
        .unwrap_err();
    assert_eq!(error.code(), nucleos::contract::ErrorCode::Invalid);
    assert!(error.reason().contains("needs a prompt"), "{error}");
}

/// The speech options are validated where they are decoded, not at the provider.
#[test]
fn speech_options_are_validated() {
    use nucleos::adapter::{Extension, ExtensionEntry, decode_extensions};
    use nucleos_openai::extension::SPEECH_DECODER;

    let mut registry = std::collections::BTreeMap::new();
    registry.insert(
        "openai/speech".to_owned(),
        SPEECH_DECODER as nucleos::adapter::ExtensionDecoder,
    );
    let decode = |fields: serde_json::Value| {
        decode_extensions(
            &[ExtensionEntry {
                provider: "openai".to_owned(),
                id: "speech".to_owned(),
                fields,
            }],
            &registry,
            "inference.extensions",
        )
    };

    // A format the provider does not encode is a configuration error here.
    let error = decode(json!({"format": "midi"})).unwrap_err();
    assert!(error.reason().contains("format must be"), "{error}");
    // So is a rate outside the provider's range.
    let error = decode(json!({"speed": 9.0})).unwrap_err();
    assert!(error.reason().contains("between 0.25 and 4.0"), "{error}");
    // And an option nobody implements is refused rather than ignored.
    let error = decode(json!({"emotion": "cheerful"})).unwrap_err();
    assert!(error.reason().contains("unknown field"), "{error}");

    let set = decode(json!({"voice": "nova", "format": "wav"})).unwrap();
    let options = set
        .for_provider("openai")
        .find_map(|extension| {
            extension
                .as_any()
                .downcast_ref::<nucleos_openai::SpeechOptions>()
        })
        .expect("the entry round-trips");
    options.validate().unwrap();
    assert_eq!(options.active_fields(), vec!["voice", "format"]);
}
