//! The first complete client flow, without pretending these types are already an HTTP server.

#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};

use nucleos_contract::content::validate_well_formed;
use nucleos_contract::wire::{ConfigureSession, OpenSession, SessionView};
use nucleos_contract::{
    AppId, Authority, Confine, Envelope, FinishReason, GenerateResponse, Message, Mode, ModelRef,
    ModelRequest, Outcome, Output, SessionId, SessionKey, SkillRootName, ToolName,
};

fn model() -> ModelRef {
    ModelRef::new("openai/gpt-5").unwrap()
}

fn authority() -> Authority {
    Authority {
        models: BTreeMap::from([(model(), BTreeSet::from([Mode::Text]))]),
        tools: BTreeSet::from([ToolName::new("read").unwrap()]),
        sandbox: Confine::default(),
        skills: BTreeSet::from([SkillRootName::new("engineering").unwrap()]),
    }
}

fn app() -> AppId {
    AppId::new("writer").unwrap()
}

#[test]
fn open_then_inference_uses_native_contract_payloads() {
    // 1. The client opens a session. No application id appears in the body: authentication owns it.
    let open = OpenSession {
        session: SessionId::new("draft-1").unwrap(),
        authority: authority(),
    };
    let open_json = serde_json::to_value(&open).unwrap();
    assert!(open_json.get("app").is_none());
    let open: OpenSession = serde_json::from_value(open_json).unwrap();

    // 2. The runtime checks the native plan against the current application envelope.
    let envelope = Envelope {
        app: app(),
        version: 42,
        authority: authority(),
    };
    envelope.validate(&open.authority).unwrap();
    let view = SessionView {
        key: SessionKey::new(app(), open.session),
        authority: open.authority,
        envelope_version: envelope.version,
    };
    let view_json = serde_json::to_value(&view).unwrap();
    assert_eq!(view_json["key"]["app"], "writer");
    assert_eq!(view_json["key"]["session"], "draft-1");
    assert_eq!(view_json["envelope_version"], 42);
    let view: SessionView = serde_json::from_value(view_json).unwrap();

    // 3. Inference selects the session in its operation path. Its body is exactly ModelRequest.
    assert!(view.authority.allows_model(&model(), Mode::Text));
    let request = ModelRequest {
        model: Some(model()),
        messages: vec![Message::user("Write one sentence.")].into(),
        output: Some(Output::Text),
        max_output_tokens: Some(80),
        ..ModelRequest::default()
    };
    validate_well_formed(&request.messages).unwrap();
    let request_json = serde_json::to_value(&request).unwrap();
    assert!(request_json.get("app").is_none());
    assert!(request_json.get("session").is_none());
    let request: ModelRequest = serde_json::from_value(request_json).unwrap();

    // 4. The unary answer is exactly GenerateResponse, including identity and accounting data.
    let response = GenerateResponse {
        model: model(),
        message: Message::assistant("One sentence."),
        finish: FinishReason::Stop,
        outcome: Outcome::default(),
        warnings: Vec::new(),
    };
    response.message.validate().unwrap();
    let response_json = serde_json::to_string(&response).unwrap();
    let decoded: GenerateResponse = serde_json::from_str(&response_json).unwrap();
    assert_eq!(decoded, response);
    assert_eq!(request.model, Some(model()));
}

#[test]
fn configure_uses_the_same_authority_and_current_envelope_check() {
    let configure = ConfigureSession {
        authority: authority(),
    };
    let json = serde_json::to_string(&configure).unwrap();
    let configure: ConfigureSession = serde_json::from_str(&json).unwrap();
    let envelope = Envelope {
        app: app(),
        version: 43,
        authority: authority(),
    };
    envelope.validate(&configure.authority).unwrap();

    let mut outside = configure.authority.clone();
    outside
        .models
        .insert(model(), BTreeSet::from([Mode::Text, Mode::Embed]));
    let error = outside.fits_in(&envelope.authority).unwrap_err();
    assert_eq!(error.field.as_deref(), Some("authority.models"));
}

#[test]
fn session_operations_and_native_inference_calls_produce_schemas() {
    for (schema, fields) in [
        (
            serde_json::to_string(&schemars::schema_for!(OpenSession)).unwrap(),
            vec!["session", "authority", "Authority"],
        ),
        (
            serde_json::to_string(&schemars::schema_for!(SessionView)).unwrap(),
            vec!["key", "authority", "envelope_version", "SessionKey"],
        ),
        (
            serde_json::to_string(&schemars::schema_for!(ConfigureSession)).unwrap(),
            vec!["authority", "Authority"],
        ),
        (
            serde_json::to_string(&schemars::schema_for!(ModelRequest)).unwrap(),
            vec!["model", "messages", "output", "max_output_tokens"],
        ),
        (
            serde_json::to_string(&schemars::schema_for!(GenerateResponse)).unwrap(),
            vec!["message", "finish", "outcome", "warnings", "Outcome"],
        ),
    ] {
        for field in fields {
            assert!(schema.contains(field), "{field} is missing from {schema}");
        }
    }
}
