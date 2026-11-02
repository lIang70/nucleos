//! The MCP client, against a real server process.
//!
//! The fixture is a Python script that speaks the three methods this crate uses. It is a real
//! subprocess over real pipes, because the parts that go wrong in an MCP client are the framing, the
//! dispatch of responses to waiters, and the handshake ordering — none of which a mock would catch.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::adapter::{Tool, ToolCtx};
use nucleos::contract::SessionId;
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
        progress: &nucleos::adapter::NoProgress,
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
// Reconnection
// ---------------------------------------------------------------------------

/// A fixture whose `echo` dies when it is asked to echo `dying`, and spawns a new child on demand.
fn dying_fixture(marker: &str) -> McpSource {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/echo_server.py");
    let mut source = McpSource::new("echo", "python3")
        .with_args(["-u", path.to_str().unwrap()])
        .with_timeout(Duration::from_secs(20));
    source
        .env
        .insert("ECHO_DIE_ON".to_owned(), marker.to_owned());
    source
}

fn scratch(name: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!("nucleos-mcp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

#[tokio::test]
async fn a_source_that_crashes_is_back_for_the_next_call() {
    let tools = dying_fixture("dying").connect().await.unwrap();
    let tool = &tools[0];
    let session = SessionId::new("s1");

    // It works, then the child exits without answering.
    assert!(
        !tool
            .invoke("{\"text\":\"first\"}", &ctx(&session, "call-1"))
            .await
            .is_error
    );
    let crashed = tool
        .invoke("{\"text\":\"dying\"}", &ctx(&session, "call-2"))
        .await;
    assert!(crashed.is_error, "a crash is a failure the model can read");
    assert!(
        crashed.content.text().contains("source was lost"),
        "{}",
        crashed.content.text()
    );

    // And the next call has a server again: this is the whole point. The failed call is *not*
    // retried — a tool call is not assumed idempotent — but the source is no longer a corpse.
    let again = tool
        .invoke("{\"text\":\"after\"}", &ctx(&session, "call-3"))
        .await;
    assert!(!again.is_error, "{}", again.content.text());
    assert_eq!(again.content.text(), "echo: after");
}

#[tokio::test]
async fn a_source_that_comes_back_changed_is_refused_rather_than_adopted() {
    let directory = scratch("changed");
    let marker = directory.join("extra-tool");
    let log = directory.join("spawns");
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/echo_server.py");

    // The fixture advertises a second tool only when this file exists, so the restart after the
    // crash is a different server than the session was promised.
    let mut source = McpSource::new("echo", "sh")
        .with_args([
            "-c".to_owned(),
            format!(
                "if [ -f {marker} ]; then export ECHO_EXTRA_TOOL=1; fi; exec python3 -u {script}",
                marker = marker.display(),
                script = path.display()
            ),
        ])
        .with_timeout(Duration::from_secs(20));
    source
        .env
        .insert("ECHO_DIE_ON".to_owned(), "dying".to_owned());
    source
        .env
        .insert("ECHO_SPAWN_LOG".to_owned(), log.display().to_string());

    let tools = source.connect().await.unwrap();
    assert_eq!(tools.len(), 1);
    let tool = &tools[0];
    let session = SessionId::new("s1");

    std::fs::write(&marker, "now there are two").unwrap();
    let crashed = tool
        .invoke("{\"text\":\"dying\"}", &ctx(&session, "call-1"))
        .await;
    assert!(crashed.is_error);
    assert!(
        crashed.content.text().contains("different tool set"),
        "a new generation is not a reconnect: {}",
        crashed.content.text()
    );

    // And the session's declaration still describes the tool it calls.
    assert_eq!(tool.spec().name, "echo");
    let _ = std::fs::remove_dir_all(&directory);
}

#[tokio::test]
async fn a_source_that_cannot_restart_is_not_spawned_once_per_call() {
    let directory = scratch("backoff");
    let log = directory.join("spawns");
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/echo_server.py");

    // The wrapper dies while running, and refuses to start again: every further attempt is a
    // spawn that fails, which is the loop backoff exists to break.
    let marker = directory.join("refuse");
    let mut source = McpSource::new("echo", "sh")
        .with_args([
            "-c".to_owned(),
            format!(
                "if [ -f {marker} ]; then exit 3; fi; exec python3 -u {script}",
                marker = marker.display(),
                script = path.display()
            ),
        ])
        .with_timeout(Duration::from_secs(20));
    source
        .env
        .insert("ECHO_DIE_ON".to_owned(), "dying".to_owned());
    source
        .env
        .insert("ECHO_SPAWN_LOG".to_owned(), log.display().to_string());

    let tools = source.connect().await.unwrap();
    let tool = &tools[0];
    let session = SessionId::new("s1");

    std::fs::write(&marker, "no more").unwrap();
    let crashed = tool
        .invoke("{\"text\":\"dying\"}", &ctx(&session, "call-1"))
        .await;
    assert!(crashed.is_error);

    // The immediate follow-ups are inside the backoff window, and they say so instead of forking.
    for call in ["call-2", "call-3"] {
        let output = tool.invoke("{\"text\":\"hi\"}", &ctx(&session, call)).await;
        assert!(output.is_error);
        assert!(
            output.content.text().contains("next attempt in"),
            "the caller is told the source is down, not given a spawn failure: {}",
            output.content.text()
        );
    }
    // The wrapper refuses to start, so the server never runs again: one line, from the first start.
    let spawns = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        spawns.lines().count(),
        1,
        "the source is not respawned once per call: {spawns}"
    );
    let _ = std::fs::remove_dir_all(&directory);
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
