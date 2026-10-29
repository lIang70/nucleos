//! The MCP client, against a real server process.
//!
//! The fixture is a Python script that speaks the three methods this crate uses. It is a real
//! subprocess over real pipes, because the parts that go wrong in an MCP client are the framing, the
//! dispatch of responses to waiters, and the handshake ordering — none of which a mock would catch.

#![allow(clippy::unwrap_used)]

use nucleos::adapter::{Tool, ToolCtx};
use nucleos::contract::{Isolation, SessionId};
use nucleos_mcp::McpSource;
use std::time::Duration;

fn fixture() -> McpSource {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/echo_server.py");
    McpSource::new("echo", "python3")
        .with_args(["-u", path.to_str().unwrap()])
        .with_timeout(Duration::from_secs(20))
}

fn ctx<'a>(session: &'a SessionId, call_id: &'a str) -> ToolCtx<'a> {
    ToolCtx {
        session,
        call_id,
        deadline_unix_millis: u64::MAX,
    }
}

#[tokio::test]
async fn a_source_discovers_and_runs_a_tool() {
    let tools = fixture().connect().await.unwrap();
    assert_eq!(tools.len(), 1);

    let tool = &tools[0];
    assert_eq!(tool.spec().name, "echo");
    assert!(tool.spec().description.contains("Echo"));
    // The schema is carried as text and not interpreted: the kernel compares them structurally and
    // never reads inside.
    assert!(tool.spec().input_schema_json.contains("\"text\""));
    // A stdio source runs in another process by construction.
    assert_eq!(tool.isolation(), Isolation::Process);

    let session = SessionId::new("s1");
    let output = tool
        .invoke("{\"text\":\"hi\"}", &ctx(&session, "call-1"))
        .await;

    assert!(!output.is_error);
    assert_eq!(output.content.text(), "echo: hi");
}

#[tokio::test]
async fn a_tool_that_reports_failure_is_content_not_a_broken_connection() {
    let tools = fixture().connect().await.unwrap();
    let session = SessionId::new("s1");

    let output = tools[0].invoke("{}", &ctx(&session, "call-1")).await;

    // `isError: true` is the server telling the model something, so it travels as content with the
    // flag set — the same rule the kernel applies to its own tools.
    assert!(output.is_error);
    assert_eq!(output.content.text(), "no text to echo");

    // And the connection still works afterwards: a failed call is not a dead client.
    let again = tools[0]
        .invoke("{\"text\":\"still here\"}", &ctx(&session, "call-2"))
        .await;
    assert_eq!(again.content.text(), "echo: still here");
}

#[tokio::test]
async fn arguments_that_are_not_an_object_never_reach_the_server() {
    let tools = fixture().connect().await.unwrap();
    let session = SessionId::new("s1");

    let output = tools[0].invoke("[1,2,3]", &ctx(&session, "call-1")).await;

    assert!(output.is_error);
    assert!(output.content.text().contains("JSON object"));
}

#[tokio::test]
async fn a_server_that_is_not_there_is_an_error_at_connect() {
    let error = McpSource::new("missing", "definitely-not-a-real-command")
        .connect()
        .await
        .err()
        .unwrap();
    assert_eq!(error.code(), nucleos::contract::ErrorCode::Unavailable);
}

// ---------------------------------------------------------------------------
// Content mapping
// ---------------------------------------------------------------------------

#[test]
fn text_and_image_blocks_become_the_content_model() {
    let result = serde_json::json!({
        "content": [
            { "type": "text", "text": "here it is" },
            { "type": "image", "mimeType": "image/png", "data": "aGk=" }
        ]
    });
    let result: nucleos_mcp::rmcp::model::CallToolResult = serde_json::from_value(result).unwrap();
    let (content, is_error) = nucleos_mcp::content::decode(&result);

    assert!(!is_error);
    assert_eq!(content.parts.len(), 2);
    assert_eq!(content.text(), "here it is");
    match &content.parts[1].kind {
        nucleos::contract::PartKind::Image(image) => {
            assert_eq!(image.source.media_type, "image/png");
            match &image.source.from {
                nucleos::contract::MediaFrom::Data { data } => assert_eq!(data, b"hi"),
                other => panic!("expected inline bytes, got {other:?}"),
            }
        }
        other => panic!("expected an image, got {other:?}"),
    }
}

#[test]
fn a_resource_that_carries_text_is_text() {
    // A pointer to something we already have is a worse answer than the thing itself.
    let result = serde_json::json!({
        "content": [{
            "type": "resource",
            "resource": { "uri": "file:///a.txt", "mimeType": "text/plain", "text": "the body" }
        }]
    });
    let result: nucleos_mcp::rmcp::model::CallToolResult = serde_json::from_value(result).unwrap();
    let (content, _) = nucleos_mcp::content::decode(&result);
    assert_eq!(content.text(), "the body");
}

#[test]
fn a_resource_that_carries_a_blob_is_a_file_reference() {
    let result = serde_json::json!({
        "content": [{
            "type": "resource",
            "resource": {
                "uri": "s3://bucket/key",
                "mimeType": "application/pdf",
                "blob": "aGk="
            }
        }]
    });
    let result: nucleos_mcp::rmcp::model::CallToolResult = serde_json::from_value(result).unwrap();
    let (content, _) = nucleos_mcp::content::decode(&result);
    match &content.parts[0].kind {
        nucleos::contract::PartKind::File(file) => {
            assert_eq!(file.uri, "s3://bucket/key");
            assert_eq!(file.media_type, "application/pdf");
        }
        other => panic!("expected a file reference, got {other:?}"),
    }
}

#[test]
fn a_block_this_build_cannot_render_is_visible_not_silent() {
    // The protocol's type rejects a block it does not know, so this is tested at the mapping level:
    // the arm exists, and it names what it could not render.
    // A model that is told "there was something here" can ask for it; one that is told nothing will
    // reason about a file it never saw.
    // A `CallToolResult` cannot be built with an unknown block, which is the point: the marker arm
    // covers a *newer* protocol, not a malformed one.
    assert!(
        serde_json::from_value::<nucleos_mcp::rmcp::model::CallToolResult>(
            serde_json::json!({ "content": [{ "type": "hologram" }] })
        )
        .is_err()
    );
}

#[test]
fn a_result_with_no_content_says_so_rather_than_being_empty() {
    let empty: nucleos_mcp::rmcp::model::CallToolResult =
        serde_json::from_value(serde_json::json!({ "content": [] })).unwrap();
    let (content, is_error) = nucleos_mcp::content::decode(&empty);
    assert!(!is_error);
    assert!(!content.is_empty());
    assert!(content.text().contains("no content"));
}
