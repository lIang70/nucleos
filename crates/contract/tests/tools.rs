//! Tool dispatch and approval are native contract payloads, including their retry relationship.

#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};

use nucleos_contract::content::{validate_callable, validate_well_formed};
use nucleos_contract::wire::{CallTools, ToolBatchResult, ToolCatalog};
use nucleos_contract::{
    Approval, ApprovalOption, ApprovalRequired, Arguments, Authority, Confine, Content, Message,
    ModelRequest, Part, Role, ToolCall, ToolDefinition, ToolName, ToolResult,
};

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.to_owned(),
        name: ToolName::new(name).unwrap(),
        arguments: Arguments::empty(),
    }
}

fn definition(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: ToolName::new(name).unwrap(),
        description: format!("Run {name}"),
        input_schema: serde_json::json!({"type": "object"}),
    }
}

fn read_authority() -> Authority {
    Authority {
        models: BTreeMap::new(),
        tools: BTreeSet::from([ToolName::new("read").unwrap()]),
        sandbox: Confine::default(),
        skills: BTreeSet::new(),
    }
}

#[test]
fn a_catalog_contains_only_native_visible_definitions() {
    let catalog = ToolCatalog {
        tools: vec![definition("read")],
    };
    catalog.tools[0].validate().unwrap();
    let json = serde_json::to_string(&catalog).unwrap();
    let decoded: ToolCatalog = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, catalog);

    let authority = read_authority();
    assert!(authority.allows_tool(&catalog.tools[0].name));
    assert!(!authority.allows_tool(&ToolName::new("write").unwrap()));
}

#[test]
fn executed_calls_return_results_and_failures_remain_model_readable() {
    let batch = CallTools {
        calls: vec![call("c1", "read"), call("c2", "read")],
        approvals: Vec::new(),
    };
    let json = serde_json::to_value(&batch).unwrap();
    assert!(json.get("approvals").is_none());
    let batch: CallTools = serde_json::from_value(json).unwrap();

    let response = ToolBatchResult {
        results: vec![
            ToolResult::ok(&batch.calls[0].id, Content::text("contents")),
            ToolResult::failed(&batch.calls[1].id, "permission denied"),
        ],
        approvals_required: Vec::new(),
    };
    assert!(!response.results[0].is_error);
    assert!(response.results[1].is_error);
    assert_eq!(response.results[1].content.as_text(), "permission denied");

    let json = serde_json::to_value(&response).unwrap();
    assert!(json.get("approvals_required").is_none());
    let decoded: ToolBatchResult = serde_json::from_value(json).unwrap();
    assert_eq!(decoded, response);
}

#[test]
fn an_approval_is_carried_by_a_retry_of_the_same_call() {
    let calls = vec![call("c1", "exec")];
    let pending = ToolBatchResult {
        results: Vec::new(),
        approvals_required: vec![ApprovalRequired {
            call_id: "c1".to_owned(),
            reason: "this command writes outside the workspace".to_owned(),
            options: vec![
                ApprovalOption {
                    id: "once".to_owned(),
                    label: "Allow this command once".to_owned(),
                },
                ApprovalOption {
                    id: "deny".to_owned(),
                    label: "Do not run it".to_owned(),
                },
            ],
        }],
    };
    let json = serde_json::to_value(&pending).unwrap();
    assert!(json.get("results").is_none());
    let pending: ToolBatchResult = serde_json::from_value(json).unwrap();

    let required = &pending.approvals_required[0];
    let retry = CallTools {
        calls,
        approvals: vec![Approval {
            id: required.call_id.clone(),
            option_id: required.options[0].id.clone(),
        }],
    };
    let json = serde_json::to_string(&retry).unwrap();
    let retry: CallTools = serde_json::from_str(&json).unwrap();
    assert_eq!(retry.approvals[0].id, retry.calls[0].id);
    assert_eq!(retry.approvals[0].option_id, "once");

    let completed = ToolBatchResult {
        results: vec![ToolResult::ok("c1", Content::text("done"))],
        approvals_required: Vec::new(),
    };
    assert!(completed.approvals_required.is_empty());
}

#[test]
fn tool_results_reenter_inference_as_caller_owned_messages() {
    let asked = Message {
        role: Role::Assistant,
        content: Content::new([Part::ToolCall {
            call: call("c1", "read"),
        }]),
        metadata: None,
    };
    let calls = asked.content.tool_calls().into_iter().cloned().collect();
    let batch = CallTools {
        calls,
        approvals: Vec::new(),
    };
    let result = ToolBatchResult {
        results: vec![ToolResult::ok(
            &batch.calls[0].id,
            Content::text("contents"),
        )],
        approvals_required: Vec::new(),
    };
    let answered = Message {
        role: Role::Tool,
        content: Content::new([Part::ToolResult {
            result: result.results[0].clone(),
        }]),
        metadata: None,
    };
    let next = ModelRequest {
        messages: vec![Message::user("Read it"), asked, answered].into(),
        ..ModelRequest::default()
    };
    validate_well_formed(&next.messages).unwrap();
    validate_callable(&next.messages, |name| name == "read").unwrap();
    let json = serde_json::to_string(&next).unwrap();
    let decoded: ModelRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, next);
}

#[test]
fn tool_operations_produce_native_schemas() {
    for (schema, fields) in [
        (
            serde_json::to_string(&schemars::schema_for!(ToolCatalog)).unwrap(),
            vec!["tools", "ToolDefinition"],
        ),
        (
            serde_json::to_string(&schemars::schema_for!(CallTools)).unwrap(),
            vec!["calls", "approvals", "ToolCall", "Approval"],
        ),
        (
            serde_json::to_string(&schemars::schema_for!(ToolBatchResult)).unwrap(),
            vec![
                "results",
                "approvals_required",
                "ToolResult",
                "ApprovalRequired",
            ],
        ),
    ] {
        for field in fields {
            assert!(schema.contains(field), "{field} is missing from {schema}");
        }
    }
}
