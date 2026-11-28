//! A schema can be generated from the contract, which is the whole premise of a published
//! document: an SDK generator reads one JSON file and never compiles this crate.

#![allow(clippy::unwrap_used)]

/// Every seam's shapes produce a schema, and nested types are reachable by name.
#[test]
fn each_seam_produces_a_schema() {
    let tool = serde_json::to_string(&schemars::schema_for!(nucleos_contract::ToolCall)).unwrap();
    for named in ["ToolCall", "Arguments"] {
        assert!(
            tool.contains(named),
            "{named} is missing from the tool schema"
        );
    }

    let sandbox = serde_json::to_string(&schemars::schema_for!(nucleos_contract::Confine)).unwrap();
    for named in ["Confine", "Files", "NetPolicy"] {
        assert!(
            sandbox.contains(named),
            "{named} is missing from the sandbox schema"
        );
    }

    let inference =
        serde_json::to_string(&schemars::schema_for!(nucleos_contract::ModelDescriptor)).unwrap();
    for named in ["ModelDescriptor", "Capabilities", "Limits"] {
        assert!(
            inference.contains(named),
            "{named} is missing from the inference schema"
        );
    }
}

/// A message's payload is tagged by kind, so a consumer that ignores the vocabulary still
/// round-trips it.
#[test]
fn a_message_schema_carries_the_part_kinds() {
    let schema = serde_json::to_string(&schemars::schema_for!(nucleos_contract::Message)).unwrap();
    for kind in ["text", "tool_call", "tool_result", "reasoning"] {
        assert!(schema.contains(kind), "{kind} is missing");
    }
}

/// Identity and authority are contract shapes too: an SDK must not have to recreate them.
#[test]
fn an_envelope_schema_reaches_the_native_resource_vocabulary() {
    let schema = serde_json::to_string(&schemars::schema_for!(nucleos_contract::Envelope)).unwrap();
    for named in [
        "Envelope",
        "Authority",
        "Confine",
        "models",
        "tools",
        "sandbox",
        "skills",
    ] {
        assert!(schema.contains(named), "{named} is missing");
    }

    let key = serde_json::to_string(&schemars::schema_for!(nucleos_contract::SessionKey)).unwrap();
    for field in ["app", "session"] {
        assert!(key.contains(field), "{field} is missing");
    }
}

/// Adapter and wire calls share one schema-bearing shape rather than parallel DTOs.
#[test]
fn shared_calls_produce_schemas() {
    let request =
        serde_json::to_string(&schemars::schema_for!(nucleos_contract::ModelRequest)).unwrap();
    for field in [
        "messages",
        "instructions",
        "tools",
        "output",
        "extensions",
        "max_output_tokens",
    ] {
        assert!(request.contains(field), "{field} is missing");
    }

    let stream =
        serde_json::to_string(&schemars::schema_for!(nucleos_contract::StreamEvent)).unwrap();
    for event in [
        "start",
        "text",
        "reasoning",
        "tool_call",
        "chunk",
        "usage",
        "end",
    ] {
        assert!(stream.contains(event), "{event} is missing");
    }

    let embedding =
        serde_json::to_string(&schemars::schema_for!(nucleos_contract::EmbedRequest)).unwrap();
    assert!(embedding.contains("dimensions"));
    let decision =
        serde_json::to_string(&schemars::schema_for!(nucleos_contract::DecisionResponse)).unwrap();
    for named in ["Probability", "predicate", "refused", "Usage"] {
        assert!(decision.contains(named), "{named} is missing");
    }
}
