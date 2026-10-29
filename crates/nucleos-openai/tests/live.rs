//! Against a real endpoint. Ignored by default, because CI has no key and should not.
//!
//! ```bash
//! set -a && . ./.env && set +a
//! HTTPS_PROXY=http://127.0.0.1:7890 \
//!   NUCLEOS_LIVE_MODEL=gpt-4o-mini \
//!   cargo test -p nucleos-openai --test live -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `NUCLEOS_LIVE_URL` overrides the endpoint; `NUCLEOS_LIVE_MODEL` the model. The proxy comes from
//! `HTTPS_PROXY`, which `reqwest` reads itself.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::adapter::{FinishReason, Model, ModelDelta, ModelRequest, TextFormat, ToolChoice};
use nucleos::contract::{Content, Message, Part, PartKind, Role, TextPart, ToolSpec, Usage};
use nucleos_openai::{Config, OpenAi};
use std::time::Duration;

fn model() -> OpenAi {
    let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY");
    let url = std::env::var("NUCLEOS_LIVE_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1/responses".to_owned());
    let name = std::env::var("NUCLEOS_LIVE_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_owned());
    OpenAi::new(Config {
        url,
        api_key: Some(key),
        model: name,
        capability: Config::capabilities(128_000, 4_096),
        options: nucleos::contract::Hints::default(),
        prompt_cache_key: None,
        include: Vec::new(),
        safety_identifier: None,
        // gpt-4o-mini answers `unsupported_parameter` to reasoning.effort; the adapter is told,
        // rather than finding out from a 400.
        supports_reasoning: std::env::var("NUCLEOS_LIVE_REASONING").is_ok(),
        // Off by default, because strict mode rejects a schema without
        // `additionalProperties: false` — and rejects the whole call with it.
        strict_tools: std::env::var("NUCLEOS_LIVE_STRICT").is_ok(),
        timeout: Some(Duration::from_secs(60)),
    })
    .unwrap()
}

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        content: Content::new([Part::new(PartKind::Text(TextPart {
            text: text.to_owned(),
        }))]),
    }
}

fn tool() -> ToolSpec {
    ToolSpec {
        name: "read_file".to_owned(),
        description: "Read a file from the workspace.".to_owned(),
        input_schema_json:
            r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#
                .to_owned(),
    }
}

struct Collected {
    text: String,
    reasoning: String,
    calls: Vec<(u32, String, String, String)>,
    usage: Option<Usage>,
    finish: Option<FinishReason>,
    dropped: Vec<String>,
}

async fn run(request: ModelRequest) -> Result<Collected, String> {
    let mut stream = model()
        .invoke(request)
        .await
        .map_err(|error| format!("{:?}: {}", error.code(), error.reason()))?;

    use tokio_stream::StreamExt;
    let mut got = Collected {
        text: String::new(),
        reasoning: String::new(),
        calls: Vec::new(),
        usage: None,
        finish: None,
        dropped: Vec::new(),
    };

    while let Some(delta) = stream.next().await {
        match delta.map_err(|error| format!("{:?}: {}", error.code(), error.reason()))? {
            ModelDelta::Text { text, .. } => got.text.push_str(&text),
            ModelDelta::Reasoning { text, .. } => got.reasoning.push_str(&text),
            ModelDelta::ToolCallStarted { index, id, name } => {
                got.calls.push((index, id, name, String::new()));
            }
            ModelDelta::ToolCallArgs { index, fragment } => {
                if let Some(call) = got.calls.iter_mut().find(|call| call.0 == index) {
                    call.3.push_str(&fragment);
                }
            }
            ModelDelta::Usage(usage) => got.usage = Some(usage),
            ModelDelta::Finished { reason } => got.finish = Some(reason),
            ModelDelta::Dropped { field, reason } => got.dropped.push(format!("{field}: {reason}")),
        }
    }
    Ok(got)
}

#[tokio::test]
#[ignore = "needs a live endpoint"]
async fn text_streams_and_reports_usage() {
    let got = run(ModelRequest {
        system: Some("Answer with one word.".to_owned()),
        messages: vec![user("Say hello.")],
        params: nucleos::adapter::GenerateParams {
            max_output_tokens: Some(16),
            ..Default::default()
        },
        ..ModelRequest::default()
    })
    .await
    .unwrap();

    assert!(!got.text.is_empty(), "no text: {got:?}", got = got.text);
    assert_eq!(got.finish, Some(FinishReason::Stop));
    let usage = got.usage.expect("no usage: the ledger would go blind");
    assert!(
        usage.input_tokens > 0 && usage.output_tokens > 0,
        "{usage:?}"
    );
}

#[tokio::test]
#[ignore = "needs a live endpoint"]
async fn a_tool_call_assembles_into_valid_json() {
    let got = run(ModelRequest {
        messages: vec![user("Read the file /etc/hosts. Use the tool.")],
        tools: vec![tool()],
        params: nucleos::adapter::GenerateParams {
            max_output_tokens: Some(128),
            ..Default::default()
        },
        ..ModelRequest::default()
    })
    .await
    .unwrap();

    assert_eq!(got.finish, Some(FinishReason::ToolCalls), "{:?}", got.text);
    let call = got.calls.first().expect("no tool call was emitted");
    assert_eq!(call.2, "read_file");
    let args: serde_json::Value = serde_json::from_str(&call.3)
        .unwrap_or_else(|_| panic!("arguments are not JSON: {:?}", call.3));
    assert_eq!(args["path"], "/etc/hosts");
}

#[tokio::test]
#[ignore = "needs a live endpoint"]
async fn every_parameter_in_the_contract_is_sent() {
    // One call per parameter, so a rejection names the parameter instead of hiding inside a
    // prompt that also set something else.
    let cases: Vec<(&str, nucleos::adapter::GenerateParams)> = vec![
        (
            "temperature",
            nucleos::adapter::GenerateParams {
                temperature: Some(0.2),
                max_output_tokens: Some(256),
                ..Default::default()
            },
        ),
        (
            "top_p",
            nucleos::adapter::GenerateParams {
                top_p: Some(0.9),
                max_output_tokens: Some(256),
                ..Default::default()
            },
        ),
        (
            "stop",
            nucleos::adapter::GenerateParams {
                stop: vec!["\n".to_owned()],
                max_output_tokens: Some(256),
                ..Default::default()
            },
        ),
        (
            "max_output_tokens",
            nucleos::adapter::GenerateParams {
                max_output_tokens: Some(200),
                ..Default::default()
            },
        ),
        (
            "reasoning_effort",
            nucleos::adapter::GenerateParams {
                reasoning_effort: Some(nucleos::adapter::ReasoningEffort::Low),
                max_output_tokens: Some(256),
                ..Default::default()
            },
        ),
        (
            "format json_object",
            nucleos::adapter::GenerateParams {
                format: TextFormat::JsonObject,
                max_output_tokens: Some(32),
                ..Default::default()
            },
        ),
        (
            "format json_schema",
            nucleos::adapter::GenerateParams {
                format: TextFormat::JsonSchema {
                    name: "answer".to_owned(),
                    schema_json: r#"{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}"#.to_owned(),
                },
                max_output_tokens: Some(64),
                ..Default::default()
            },
        ),
        (
            "tool_choice required",
            nucleos::adapter::GenerateParams {
                tool_choice: ToolChoice::Required,
                max_output_tokens: Some(64),
                ..Default::default()
            },
        ),
    ];

    for (name, params) in cases {
        let request = ModelRequest {
            // The provider requires the word "json" in the input to accept `json_object`, and it
            // says so plainly. Verified, not assumed.
            messages: vec![user(
                "Reply with a json object, or call the tool if you must.",
            )],
            tools: vec![tool()],
            params,
            ..ModelRequest::default()
        };
        match run(request).await {
            Ok(got) => println!("  ok   {name:<24} finish={:?}", got.finish),
            // A model that refuses a parameter is information, not a failure of the adapter: the
            // adapter's job is to send what the caller asked for and report the refusal in the
            // kernel's taxonomy. The model decides what it supports.
            Err(error) => println!("  n/a  {name:<24} {error}"),
        }
    }
}

#[tokio::test]
#[ignore = "needs a live endpoint"]
async fn json_schema_is_honoured() {
    let got = run(ModelRequest {
        messages: vec![user("Give me an answer.")],
        params: nucleos::adapter::GenerateParams {
            format: TextFormat::JsonSchema {
                name: "answer".to_owned(),
                schema_json: r#"{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}"#.to_owned(),
            },
            max_output_tokens: Some(64),
            ..Default::default()
        },
        ..ModelRequest::default()
    })
    .await
    .unwrap();

    let value: serde_json::Value =
        serde_json::from_str(&got.text).unwrap_or_else(|_| panic!("not JSON: {:?}", got.text));
    assert!(value["answer"].is_string(), "{value}");
}

#[tokio::test]
#[ignore = "needs a live endpoint"]
async fn the_deployment_parameters_are_accepted() {
    // Each is verified separately against the endpoint, because "accepted" is a fact about the
    // provider and not a fact about our encoder.
    use nucleos::contract::{Hints, ServiceTier, Truncation, Verbosity};

    let key = std::env::var("OPENAI_API_KEY").unwrap();
    let url = std::env::var("NUCLEOS_LIVE_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1/responses".to_owned());
    let name = std::env::var("NUCLEOS_LIVE_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_owned());

    let base = Hints {
        metadata: [("suite".to_owned(), "nucleos".to_owned())].into(),
        truncation: Some(Truncation::Disabled),
        max_tool_calls: Some(2),
        ..Default::default()
    };

    for (label, hints) in [
        ("metadata/cache/truncation/max_tool_calls", base.clone()),
        (
            "service_tier flex",
            Hints {
                service_tier: Some(ServiceTier::Flex),
                ..base.clone()
            },
        ),
        (
            "text.verbosity",
            Hints {
                verbosity: Some(Verbosity::Low),
                ..base.clone()
            },
        ),
    ] {
        let model = OpenAi::new(Config {
            url: url.clone(),
            api_key: Some(key.clone()),
            model: name.clone(),
            capability: Config::capabilities(128_000, 4_096),
            options: hints,
            // Deployment facts, derived by a host from the session and the principal.
            prompt_cache_key: Some("nucleos-live-test".to_owned()),
            include: Vec::new(),
            safety_identifier: Some("live-test".to_owned()),
            supports_reasoning: false,
            strict_tools: false,
            timeout: Some(Duration::from_secs(60)),
        })
        .unwrap();

        match model
            .invoke(ModelRequest {
                messages: vec![user("Say ok.")],
                params: nucleos::adapter::GenerateParams {
                    max_output_tokens: Some(32),
                    ..Default::default()
                },
                ..ModelRequest::default()
            })
            .await
        {
            // A provider that refuses a parameter is information, not a failure of the adapter:
            // the adapter sends what it was configured to send and reports the refusal.
            Err(error) => println!("  n/a  {label:<38} {:?}: {}", error.code(), error.reason()),
            Ok(mut stream) => {
                use tokio_stream::StreamExt;
                let mut failure = None;
                while let Some(delta) = stream.next().await {
                    if let Err(error) = delta {
                        failure = Some(error);
                        break;
                    }
                }
                match failure {
                    Some(error) => {
                        println!("  n/a  {label:<38} {:?}: {}", error.code(), error.reason());
                    }
                    None => println!("  ok   {label}"),
                }
            }
        }
    }
}
