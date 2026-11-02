//! One turn, delivered as it happens.
//!
//! Two claims. The frames are the session's *events* — the same bytes a replay would give — so a
//! client cannot learn from the stream anything the log does not say. And the last frame is the
//! `message_completed` the kernel already wrote, so a client that ignores every earlier frame ends up
//! with exactly what the non-streaming call returns.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use nucleos::adapter::{AllowAll, Store};
use nucleos::contract::{Principal, Routing};
use nucleos::kernel::Kernel;
use nucleosd::{App, router};
use std::sync::Arc;
use tower::ServiceExt;

fn app() -> Arc<App> {
    let kernel = Kernel::builder()
        .model(
            "primary",
            Arc::new(nucleos_testkit::FakeModel::text(["你好, world"])),
        )
        .store(Arc::new(nucleos_sqlite::SqliteStore::in_memory().unwrap()) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap();
    Arc::new(App::new(
        Arc::new(kernel),
        Principal::default(),
        vec!["primary".to_owned()],
    ))
}

fn ask(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/sessions/s1/inference")
        .header("accept", "text/event-stream")
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

#[tokio::test]
async fn a_turn_streams_its_events_and_ends_on_the_completed_message() {
    let app = app();
    let created = router(Arc::clone(&app))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/sessions")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"session":"s1","credit_tokens":1000000}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::OK);

    let response = router(Arc::clone(&app))
        .oneshot(ask(
            r#"{"messages":[{"role":"user","content":{"parts":[{"kind":"text","text":"hi"}]}}]}"#,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );

    let body = String::from_utf8(
        axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();

    let frames = frames(&body);
    let kinds: Vec<&str> = frames
        .iter()
        .map(|frame| frame.event.as_deref().unwrap_or(""))
        .collect();

    // The first frame is the one the kernel writes before it calls the model: a turn that has not
    // answered yet is distinguishable from one that never started.
    assert_eq!(kinds.first(), Some(&"nucleos.v1.turn_started"), "{kinds:?}");
    assert!(kinds.contains(&"nucleos.v1.text_delta"), "{kinds:?}");
    // The last frame is the kernel's own `message_completed`, not a frame invented by the transport.
    assert_eq!(
        kinds.last(),
        Some(&"nucleos.v1.message_completed"),
        "{kinds:?}"
    );

    // Every `data` is a whole event, and the sequence the client saw is the log's sequence: strictly
    // increasing, which is what makes `Last-Event-ID` a cursor it can resume on.
    let events: Vec<serde_json::Value> = frames
        .iter()
        .map(|frame| serde_json::from_str(&frame.data).expect("a frame is an event"))
        .collect();
    let seqs: Vec<u64> = events
        .iter()
        .map(|event| event["seq"].as_u64().expect("an event has a seq"))
        .collect();
    assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");
    for (event, frame) in events.iter().zip(&frames) {
        assert_eq!(event["seq"].to_string(), frame.id.clone().unwrap());
        assert_eq!(
            event["kind"]
                .as_str()
                .map(|kind| format!("nucleos.v1.{kind}")),
            frame.event,
            "the SSE event name is derived from the JSON tag, so they cannot disagree"
        );
    }

    // What the terminal frame says is what the non-streaming call would have returned.
    let terminal = events.last().unwrap();
    assert_eq!(terminal["kind"], "message_completed");
    assert_eq!(terminal["message"]["role"], "assistant", "{}", terminal);
}

#[tokio::test]
async fn a_turn_refused_before_it_starts_keeps_its_status_code() {
    let app = app();
    router(Arc::clone(&app))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/sessions")
                // No credit: `spend` refuses before `turn_started` exists, so there is nothing to
                // stream and the caller is owed a status code rather than a 200 with an error frame.
                .header("content-type", "application/json")
                .body(Body::from(r#"{"session":"s1","credit_tokens":0}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let response = router(Arc::clone(&app))
        .oneshot(ask(
            r#"{"messages":[{"role":"user","content":{"parts":[{"kind":"text","text":"hi"}]}}]}"#,
        ))
        .await
        .unwrap();

    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{status}");
    assert!(
        !content_type.starts_with("text/event-stream"),
        "{content_type}"
    );
    let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["code"], "budget", "{error}");
}

#[tokio::test]
async fn a_streaming_request_that_never_asked_for_a_stream_still_answers_json() {
    // The old shape is untouched: same route, same body, no `Accept`.
    let app = app();
    router(Arc::clone(&app))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/sessions")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"session":"s1","credit_tokens":1000000}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let response = router(Arc::clone(&app))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/sessions/s1/inference")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"messages":[{"role":"user","content":{"parts":[{"kind":"text","text":"hi"}]}}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["message"]["role"], "assistant", "{body}");
    assert!(body["finish"].is_string(), "{body}");
}

struct Frame {
    id: Option<String>,
    event: Option<String>,
    data: String,
}

/// Split an SSE body into frames: `field: value` lines, frames separated by a blank line.
fn frames(body: &str) -> Vec<Frame> {
    body.split("\n\n")
        .filter(|block| !block.trim().is_empty())
        // Keep-alive comments are frames with no data; they are the daemon saying it is alive.
        .filter(|block| !block.trim_start().starts_with(':'))
        .map(|block| {
            let mut frame = Frame {
                id: None,
                event: None,
                data: String::new(),
            };
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("id: ") {
                    frame.id = Some(value.to_owned());
                } else if let Some(value) = line.strip_prefix("event: ") {
                    frame.event = Some(value.to_owned());
                } else if let Some(value) = line.strip_prefix("data: ") {
                    frame.data.push_str(value);
                }
            }
            frame
        })
        .collect()
}

/// A model that takes its time, so a client can hang up while the turn is still running.
struct Slow;

#[async_trait::async_trait]
impl nucleos::adapter::Model for Slow {
    fn capability(&self) -> nucleos::contract::Capability {
        nucleos::contract::Capability {
            tools: true,
            tool_choice_required: true,
            structured_output: true,
            streaming: true,
            context_window: 128_000,
            max_output_tokens: 4_096,
        }
    }

    fn count_tokens(
        &self,
        request: &nucleos::adapter::ModelRequest,
    ) -> nucleos::error::Result<u64> {
        Ok(request.text_len().div_ceil(4).max(1))
    }

    async fn invoke(
        &self,
        _request: nucleos::adapter::ModelRequest,
    ) -> nucleos::error::Result<nucleos::adapter::ModelStream> {
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        tokio::spawn(async move {
            // Long enough that the client can hang up first; a turn that finished before the drop
            // would make this test about a race rather than about disconnecting.
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            let _ = sender
                .send(Ok(nucleos::adapter::ModelDelta::Text {
                    part_index: 0,
                    text: "too late".to_owned(),
                }))
                .await;
            let _ = sender
                .send(Ok(nucleos::adapter::ModelDelta::Finished {
                    reason: nucleos::adapter::FinishReason::Stop,
                }))
                .await;
        });
        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(
            receiver,
        )))
    }
}

fn slow_app() -> Arc<App> {
    let kernel = Kernel::builder()
        .model("primary", Arc::new(Slow))
        .store(Arc::new(nucleos_sqlite::SqliteStore::in_memory().unwrap()) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap();
    Arc::new(App::new(
        Arc::new(kernel),
        Principal::default(),
        vec!["primary".to_owned()],
    ))
}

#[tokio::test]
async fn a_client_that_hangs_up_cancels_the_turn_and_the_record_says_so() {
    use http_body_util::BodyExt as _;

    let app = slow_app();
    router(Arc::clone(&app))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/sessions")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"session":"s1","credit_tokens":1000000}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let response = router(Arc::clone(&app))
        .oneshot(ask(
            r#"{"messages":[{"role":"user","content":{"parts":[{"kind":"text","text":"hi"}]}}]}"#,
        ))
        .await
        .unwrap();
    let mut body = response.into_body();

    // Read one frame — `turn_started` — and hang up. This is what a closed tab looks like from the
    // server: the body is dropped and nothing else announces it.
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
        .await
        .expect("a frame arrives before the model answers")
        .expect("the body is not empty")
        .unwrap();
    assert!(
        String::from_utf8_lossy(&frame.into_data().unwrap()).contains("turn_started"),
        "the first frame is the announcement"
    );
    drop(body);

    // The turn outlives the connection on purpose: it must still write down that it was cancelled.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let events = router(Arc::clone(&app))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/sessions/s1/events?from_seq=0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = String::from_utf8(
        axum::body::to_bytes(events.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(body.contains("attempt_failed"), "{body}");
    assert!(body.contains("cancelled"), "{body}");

    // Cancelling stops the turn, it does not end the conversation.
    let session = router(Arc::clone(&app))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/sessions/s1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let view = body_json(session).await;
    assert_eq!(view["state"], "open", "{view}");
}

#[tokio::test]
async fn a_turn_that_fails_inside_the_call_ends_with_the_reason() {
    // The policy refuses the model, so the turn begins (budget spent, route resolved) and then dies
    // inside the call. Before `turn_failed` existed the stream simply ended here: a client could not
    // tell a refusal from a connection that had dropped mid-answer.
    let kernel = Kernel::builder()
        .model(
            "primary",
            Arc::new(nucleos_testkit::FakeModel::text(["never"])),
        )
        .store(Arc::new(nucleos_sqlite::SqliteStore::in_memory().unwrap()) as Arc<dyn Store>)
        .policy(Arc::new(nucleos::adapter::DenyAll))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap();
    let app = Arc::new(App::new(
        Arc::new(kernel),
        Principal::default(),
        vec!["primary".to_owned()],
    ));
    router(Arc::clone(&app))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/sessions")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"session":"s1","credit_tokens":1000000}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let response = router(Arc::clone(&app))
        .oneshot(ask(
            r#"{"messages":[{"role":"user","content":{"parts":[{"kind":"text","text":"hi"}]}}]}"#,
        ))
        .await
        .unwrap();

    // The policy is consulted per model call, not per route, so the turn begins and then dies inside
    // it. Before `turn_failed` existed this stream simply stopped after `turn_started` — a client
    // could not tell a refusal from a connection that dropped mid-answer.
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(
        axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let frames = frames(&body);
    let kinds: Vec<&str> = frames
        .iter()
        .map(|frame| frame.event.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(
        kinds,
        vec!["nucleos.v1.turn_started", "nucleos.v1.turn_failed"],
        "{kinds:?}"
    );
    let terminal: serde_json::Value = serde_json::from_str(&frames[1].data).unwrap();
    assert_eq!(terminal["code"], "denied", "{terminal}");
    assert!(
        terminal["reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "the frame says why: {terminal}"
    );
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap()
}
