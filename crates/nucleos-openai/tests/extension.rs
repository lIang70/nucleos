//! The `openai.generation` extension: per-call overrides of the deployment's OpenAI defaults.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::adapter::ModelRequest;
use nucleos::adapter::{ExtensionEntry, decode_extensions};
use nucleos::contract::{Content, Message, Part, PartKind, Role, TextPart};
use nucleos_openai::{GenerationOptions, extension::DECODER};

fn request() -> ModelRequest {
    ModelRequest {
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

fn registry() -> std::collections::BTreeMap<String, nucleos::adapter::ExtensionDecoder> {
    let mut registry = std::collections::BTreeMap::new();
    registry.insert(
        "openai/generation".to_owned(),
        DECODER as nucleos::adapter::ExtensionDecoder,
    );
    registry
}

/// The extension decodes into a typed value whose fields are qualified identities.
#[test]
fn an_entry_decodes_into_qualified_fields() {
    let set = decode_extensions(
        &[ExtensionEntry {
            provider: "openai".to_owned(),
            id: "generation".to_owned(),
            fields: serde_json::json!({
                "prompt_cache_key": "session-1",
                "include": ["reasoning_encrypted_content"],
            }),
        }],
        &registry(),
        "inference.extensions",
    )
    .unwrap();

    let fields: Vec<String> = set
        .active_fields()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(fields.contains(&"extension.openai.generation.prompt_cache_key".to_owned()));
    assert!(fields.contains(&"extension.openai.generation.include".to_owned()));
    assert!(
        !fields.contains(&"extension.openai.generation.safety_identifier".to_owned()),
        "an unset knob is not an active field: {fields:?}"
    );
}

/// The per-call value overrides the deployment default, and `include` replaces rather than appends.
#[test]
fn a_per_call_extension_overrides_the_deployment_default() {
    let mut options = request();
    options.extensions = decode_extensions(
        &[ExtensionEntry {
            provider: "openai".to_owned(),
            id: "generation".to_owned(),
            fields: serde_json::json!({
                "prompt_cache_key": "call-key",
                "include": ["reasoning_encrypted_content"],
            }),
        }],
        &registry(),
        "inference.extensions",
    )
    .unwrap();

    let entry = options
        .extensions
        .for_provider("openai")
        .find_map(|extension| extension.as_any().downcast_ref::<GenerationOptions>())
        .expect("the extension round-trips through the trait object");
    assert_eq!(entry.prompt_cache_key.as_deref(), Some("call-key"));
    assert_eq!(entry.include.len(), 1);
}

/// An unknown field is a validation error rather than a silently ignored key.
#[test]
fn an_unknown_option_is_refused() {
    let error = decode_extensions(
        &[ExtensionEntry {
            provider: "openai".to_owned(),
            id: "generation".to_owned(),
            fields: serde_json::json!({"prompt_cache_keys": "typo"}),
        }],
        &registry(),
        "inference.extensions",
    )
    .unwrap_err();
    assert!(error.reason().contains("openai.generation"), "{error}");
}

/// A per-call option reaches the wire.
///
/// This is the regression test for a defect the contract check could not see: `prepare` compiled
/// with the call's settings and `invoke` compiled again with the deployment's, so every
/// `openai.generation` option was reported as honoured and then dropped. The check passed because
/// the plan described a request that was never sent.
#[test]
fn a_per_call_option_reaches_the_body_the_plan_describes() {
    use nucleos::adapter::Model;

    let mut options = request();
    options.extensions = decode_extensions(
        &[ExtensionEntry {
            provider: "openai".to_owned(),
            id: "generation".to_owned(),
            fields: serde_json::json!({
                "prompt_cache_key": "call-key",
                "safety_identifier": "call-user",
                "include": ["reasoning_encrypted_content"],
            }),
        }],
        &registry(),
        "inference.extensions",
    )
    .unwrap();

    let adapter = nucleos_openai::OpenAi::new(
        nucleos_openai::Config {
            url: "https://example.test/v1/responses".to_owned(),
            api_key: None,
            model: "gpt-test".to_owned(),
            timeout: None,
            images_url: None,
            speech_url: None,
        },
        nucleos_openai::OpenAiSettings {
            prompt_cache_key: Some("deployment-key".to_owned()),
            safety_identifier: Some("deployment-user".to_owned()),
            ..nucleos_openai::OpenAiSettings::default()
        },
    )
    .unwrap();

    let prepared = adapter.prepare(options).unwrap();

    // What the plan claims: the extension's fields are honoured, not merely present.
    let honoured: Vec<String> = prepared
        .plan()
        .honoured
        .iter()
        .map(nucleos::contract::Field::as_str)
        .collect();
    for field in [
        "extension.openai.generation.prompt_cache_key",
        "extension.openai.generation.safety_identifier",
        "extension.openai.generation.include",
    ] {
        assert!(
            honoured.iter().any(|claimed| claimed == field),
            "{field} is not claimed as honoured: {honoured:?}"
        );
    }

    // What the call would send: the same values. The assertion is on the compiled artifact, which
    // is the body `invoke` posts — not on a second encode that could disagree with it.
    let body = &prepared
        .artifact::<nucleos_openai::wire::Encoded>()
        .expect("the adapter's compiled form is the wire body")
        .body;
    assert_eq!(
        body.get("prompt_cache_key").and_then(|v| v.as_str()),
        Some("call-key"),
        "the per-call cache key did not reach the body: {body}"
    );
    assert_eq!(
        body.get("safety_identifier").and_then(|v| v.as_str()),
        Some("call-user")
    );
    let include = body.get("include").and_then(|v| v.as_array()).unwrap();
    assert!(
        include
            .iter()
            .any(|v| v.as_str() == Some("reasoning.encrypted_content")),
        "the per-call include list did not reach the body: {body}"
    );
}

/// The kernel's correlation data gives an adapter a session-scoped default without making it a
/// session configuration knob.
#[test]
fn correlation_supplies_session_scoped_defaults() {
    use nucleos::adapter::Model;

    let mut options = request();
    options.correlation = Some(std::sync::Arc::new(nucleos::adapter::Correlation {
        session: "s1".to_owned(),
        turn: 3,
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }));

    let adapter = nucleos_openai::OpenAi::new(
        nucleos_openai::Config {
            url: "https://example.test/v1/responses".to_owned(),
            api_key: None,
            model: "gpt-test".to_owned(),
            timeout: None,
            images_url: None,
            speech_url: None,
        },
        nucleos_openai::OpenAiSettings::default(),
    )
    .unwrap();
    let prepared = adapter.prepare(options.clone()).unwrap();
    let body = &prepared
        .artifact::<nucleos_openai::wire::Encoded>()
        .unwrap()
        .body;
    assert_eq!(
        body.get("prompt_cache_key").and_then(|v| v.as_str()),
        Some("acme/s1"),
        "the cache key should follow the session, not the process: {body}"
    );
    assert_eq!(
        body.get("safety_identifier").and_then(|v| v.as_str()),
        Some("dea4eaeaeaa2ce73be2c366f5d0ea013f6752d12966a10583f32acf17d97d403"),
        "the safety identifier should be derived from the subject, not sent as the subject"
    );

    // An explicit deployment value still wins over the derived default.
    let adapter = nucleos_openai::OpenAi::new(
        nucleos_openai::Config {
            url: "https://example.test/v1/responses".to_owned(),
            api_key: None,
            model: "gpt-test".to_owned(),
            timeout: None,
            images_url: None,
            speech_url: None,
        },
        nucleos_openai::OpenAiSettings {
            prompt_cache_key: Some("deployment-key".to_owned()),
            safety_identifier: Some("deployment-user".to_owned()),
            ..nucleos_openai::OpenAiSettings::default()
        },
    )
    .unwrap();
    let prepared = adapter.prepare(options).unwrap();
    let body = &prepared
        .artifact::<nucleos_openai::wire::Encoded>()
        .unwrap()
        .body;
    assert_eq!(
        body.get("prompt_cache_key").and_then(|v| v.as_str()),
        Some("deployment-key")
    );
    assert_eq!(
        body.get("safety_identifier").and_then(|v| v.as_str()),
        Some("deployment-user")
    );
}
