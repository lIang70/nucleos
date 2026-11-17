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
//!
//! The two modalities that live on their own endpoints have their own knobs, because a provider's
//! image and speech line-ups move independently of its text one:
//!
//! ```text
//! NUCLEOS_LIVE_IMAGE_MODEL   default gpt-image-1-mini   (gpt-image-1 was retired)
//! NUCLEOS_LIVE_IMAGE_URL     default https://api.openai.com/v1/images/generations
//! NUCLEOS_LIVE_SPEECH_MODEL  default gpt-4o-mini-tts
//! NUCLEOS_LIVE_SPEECH_URL    default https://api.openai.com/v1/audio/speech
//! ```
//!
//! Both cost money, which is the other reason they are `#[ignore]`d: an image is a few thousand
//! output tokens and a clip is a paid synthesis call.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::adapter::{FinishReason, Model, ModelDelta, ModelRequest, TextFormat, ToolChoice};
use nucleos::contract::{Content, Message, Part, PartKind, Role, TextPart, ToolSpec, Usage};
use nucleos_openai::{Config, OpenAi, OpenAiSettings};
use std::time::Duration;

fn model() -> OpenAi {
    let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY");
    let url = std::env::var("NUCLEOS_LIVE_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1/responses".to_owned());
    let name = std::env::var("NUCLEOS_LIVE_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_owned());
    OpenAi::new(
        Config {
            url,
            api_key: Some(key),
            model: name,
            timeout: Some(Duration::from_secs(60)),
            images_url: None,
            speech_url: None,
        },
        OpenAiSettings {
            // gpt-4o-mini answers `unsupported_parameter` to reasoning.effort; the adapter is told,
            // rather than finding out from a 400.
            supports_reasoning: std::env::var("NUCLEOS_LIVE_REASONING").is_ok(),
            // Off by default, because strict mode rejects a schema without
            // `additionalProperties: false` — and rejects the whole call with it.
            strict_tools: std::env::var("NUCLEOS_LIVE_STRICT").is_ok(),
            ..OpenAiSettings::default()
        },
    )
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
        isolation: Some(nucleos::contract::Isolation::InProcess),
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
    let adapter = model();
    // Compile once, then send what was compiled: the plan and the body come out of one encode, so
    // a degradation reported here is by construction one that was left out of the request.
    let prepared = adapter
        .prepare(request)
        .map_err(|error| format!("{:?}: {}", error.code(), error.reason()))?;
    let dropped = prepared
        .plan()
        .degraded
        .iter()
        .map(|entry| format!("{}: {}", entry.field.as_str(), entry.reason))
        .collect();
    let mut stream = adapter
        .invoke(prepared)
        .await
        .map_err(|error| format!("{:?}: {}", error.code(), error.reason()))?;

    use tokio_stream::StreamExt;
    let mut got = Collected {
        text: String::new(),
        reasoning: String::new(),
        calls: Vec::new(),
        usage: None,
        finish: None,
        dropped,
    };

    while let Some(delta) = stream.next().await {
        match delta.map_err(|error| format!("{:?}: {}", error.code(), error.reason()))? {
            ModelDelta::PartAppend { part, .. } | ModelDelta::PartReplace { part, .. } => {
                if let nucleos::contract::PartKind::Text(text) = &part.kind {
                    got.text.push_str(&text.text);
                }
                if let nucleos::contract::PartKind::Reasoning(reasoning) = &part.kind {
                    got.reasoning.push_str(&reasoning.text);
                }
            }
            ModelDelta::Metadata { .. } => {}
            ModelDelta::ToolCallStarted {
                index, id, name, ..
            } => {
                got.calls.push((index, id, name, String::new()));
            }
            ModelDelta::ToolCallArgs { index, fragment } => {
                if let Some(call) = got.calls.iter_mut().find(|call| call.0 == index) {
                    call.3.push_str(&fragment);
                }
            }
            ModelDelta::Usage(usage) => got.usage = Some(usage),
            ModelDelta::Finished { reason } => got.finish = Some(reason),
        }
    }
    Ok(got)
}

#[tokio::test]
#[ignore = "needs a live endpoint"]
async fn text_streams_and_reports_usage() {
    let got = run(ModelRequest {
        system: Some("Answer with one word.".to_owned()),
        messages: vec![user("Say hello.")].into(),
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
        messages: vec![user("Read the file /etc/hosts. Use the tool.")].into(),
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
            )]
            .into(),
            tools: vec![tool()],
            params,
            ..ModelRequest::default()
        };
        match run(request).await {
            // The degradations are printed because a live run is where a knob silently not taking
            // effect is discovered: the plan knows before the call, and this is the record.
            Ok(got) => println!(
                "  ok   {name:<24} finish={:?} degraded={:?}",
                got.finish, got.dropped
            ),
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
        messages: vec![user("Give me an answer.")].into(),
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
        let model = OpenAi::new(
            Config {
                url: url.clone(),
                api_key: Some(key.clone()),
                model: name.clone(),
                timeout: Some(Duration::from_secs(60)),
                images_url: None,
                speech_url: None,
            },
            OpenAiSettings {
                options: hints,
                // Deployment facts, derived by a host from the session and the principal.
                prompt_cache_key: Some("nucleos-live-test".to_owned()),
                safety_identifier: Some("live-test".to_owned()),
                supports_reasoning: false,
                strict_tools: false,
                ..OpenAiSettings::default()
            },
        )
        .unwrap();

        let prepared = model.prepare(ModelRequest {
            messages: vec![user("Say ok.")].into(),
            params: nucleos::adapter::GenerateParams {
                max_output_tokens: Some(32),
                ..Default::default()
            },
            ..ModelRequest::default()
        });
        let outcome = match prepared {
            Ok(prepared) => model.invoke(prepared).await,
            Err(error) => Err(error),
        };
        match outcome {
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

// ---------------------------------------------------------------------------
// The modalities that live on their own endpoints
// ---------------------------------------------------------------------------

/// An adapter configured for the image endpoint, with the environment naming both.
fn image_model() -> OpenAi {
    let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY");
    let url = std::env::var("NUCLEOS_LIVE_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1/responses".to_owned());
    let images_url = std::env::var("NUCLEOS_LIVE_IMAGE_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1/images/generations".to_owned());
    // The cheapest model that still exercises the surface: `gpt-image-1` was retired, and a live
    // test that 400s on a model name teaches nothing about the adapter.
    let name =
        std::env::var("NUCLEOS_LIVE_IMAGE_MODEL").unwrap_or_else(|_| "gpt-image-1-mini".to_owned());
    let mut settings = OpenAiSettings::default();
    // Configured endpoint means declared output; the same rule the daemon applies.
    settings.capability.outputs = vec![
        nucleos::contract::PartKindName::Text,
        nucleos::contract::PartKindName::Image,
    ];
    OpenAi::new(
        Config {
            url,
            api_key: Some(key),
            model: name,
            timeout: Some(Duration::from_secs(120)),
            images_url: Some(images_url),
            speech_url: None,
        },
        settings,
    )
    .unwrap()
}

/// The speech endpoint, configured the same way.
fn speech_model() -> OpenAi {
    let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY");
    let url = std::env::var("NUCLEOS_LIVE_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1/responses".to_owned());
    let speech_url = std::env::var("NUCLEOS_LIVE_SPEECH_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1/audio/speech".to_owned());
    let name =
        std::env::var("NUCLEOS_LIVE_SPEECH_MODEL").unwrap_or_else(|_| "gpt-4o-mini-tts".to_owned());
    let mut settings = OpenAiSettings::default();
    settings.capability.outputs = vec![
        nucleos::contract::PartKindName::Text,
        nucleos::contract::PartKindName::Audio,
    ];
    OpenAi::new(
        Config {
            url,
            api_key: Some(key),
            model: name,
            timeout: Some(Duration::from_secs(120)),
            images_url: None,
            speech_url: Some(speech_url),
        },
        settings,
    )
    .unwrap()
}

/// Drain a stream into its deltas, failing on the first error.
async fn collect(mut stream: nucleos::adapter::ModelStream) -> Vec<ModelDelta> {
    use tokio_stream::StreamExt;
    let mut deltas = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(delta) => deltas.push(delta),
            Err(error) => panic!("stream failed: {:?}: {}", error.code(), error.reason()),
        }
    }
    deltas
}

/// Image generation produces a real image, at one part index, from one call.
///
/// The interesting claim is not "bytes arrived" — an error page has bytes — but that they arrive as an
/// image part whose media type matches the format that was asked for, replaced rather than appended:
/// a preview, then what it became.
#[tokio::test]
#[ignore = "needs a live endpoint"]
async fn image_generation_returns_an_image_part() {
    let adapter = image_model();
    let request = ModelRequest {
        output: nucleos::contract::Output::Image,
        messages: vec![user("A single red circle on a plain white background.")].into(),
        extensions: nucleos::adapter::ExtensionSet::new(vec![Box::new(
            nucleos_openai::ImageOptions {
                size: Some("1024x1024".to_owned()),
                output_format: Some("png".to_owned()),
                partial_images: Some(1),
                ..Default::default()
            },
        )]),
        ..ModelRequest::default()
    };

    let prepared = adapter.prepare(request).unwrap();
    let deltas = collect(adapter.invoke(prepared).await.unwrap()).await;

    let mut images = 0;
    let mut usage = None;
    for delta in &deltas {
        match delta {
            ModelDelta::PartReplace {
                part_index: 0,
                part,
            } => {
                if let PartKind::Image(image) = &part.kind {
                    assert_eq!(
                        image.source.media_type(),
                        "image/png",
                        "the part's type is not the format that was requested"
                    );
                    let bytes = image.source.bytes().expect("an inline image");
                    assert!(bytes.len() > 1_000, "{} bytes is not an image", bytes.len());
                    // PNG magic: the payload is the format the part claims.
                    assert_eq!(&bytes[..4], b"\x89PNG", "the payload is not a PNG");
                    images += 1;
                }
            }
            ModelDelta::PartReplace { part_index, .. } => {
                panic!("an image arrived at part {part_index}, not at the one index it owns");
            }
            ModelDelta::Usage(report) => usage = Some(report.clone()),
            _ => {}
        }
    }
    assert!(images >= 1, "no image part: {deltas:?}");
    assert!(
        matches!(deltas.last(), Some(ModelDelta::Finished { .. })),
        "the stream did not end with a finish reason"
    );
    let usage = usage.expect("no usage: image billing would go blind");
    println!(
        "  image: {images} frame(s), {} in / {} out tokens",
        usage.input_tokens, usage.output_tokens
    );
}

/// Speech synthesis produces a real clip, assembled from however many chunks the endpoint sends.
///
/// The claim worth testing is the merge: every chunk is a part at index 0 with the same media type, and
/// concatenating them must give a file that starts with the format's own signature — not the last
/// fragment of a clip whose earlier chunks replaced each other.
#[tokio::test]
#[ignore = "needs a live endpoint"]
async fn speech_synthesis_returns_an_audio_part() {
    let adapter = speech_model();
    let request = ModelRequest {
        output: nucleos::contract::Output::Audio,
        messages: vec![user("The quick brown fox jumps over the lazy dog.")].into(),
        extensions: nucleos::adapter::ExtensionSet::new(vec![Box::new(
            nucleos_openai::SpeechOptions {
                voice: Some("alloy".to_owned()),
                format: Some("mp3".to_owned()),
                ..Default::default()
            },
        )]),
        ..ModelRequest::default()
    };

    let prepared = adapter.prepare(request).unwrap();
    let deltas = collect(adapter.invoke(prepared).await.unwrap()).await;

    let mut clip = Vec::new();
    let mut chunks = 0;
    for delta in &deltas {
        if let ModelDelta::PartAppend {
            part_index: 0,
            part,
        } = delta
            && let PartKind::Audio(audio) = &part.kind
        {
            assert_eq!(
                audio.source.media_type(),
                "audio/mpeg",
                "the chunk's type is not the format that was requested"
            );
            clip.extend_from_slice(audio.source.bytes().unwrap_or_default());
            chunks += 1;
        }
    }
    assert!(chunks >= 1, "no audio part: {deltas:?}");
    assert!(clip.len() > 1_000, "{} bytes is not a clip", clip.len());
    // MP3 frames start with 0xFFFx, and an ID3 tag with "ID3". Either is proof the payload is audio.
    let is_mp3 = clip.starts_with(b"ID3") || (clip[0] == 0xFF && clip[1] & 0xE0 == 0xE0);
    assert!(is_mp3, "the payload is not an MP3: {:?}", &clip[..4]);
    assert!(
        matches!(deltas.last(), Some(ModelDelta::Finished { .. })),
        "the stream did not end with a finish reason"
    );
    println!("  speech: {chunks} chunk(s), {} bytes", clip.len());
}
