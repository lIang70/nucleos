//! The router, exercised without a socket.
//!
//! What matters here is the edge: an unconfigured daemon must describe itself and refuse to
//! pretend, and a failure must come back as the one error body every endpoint shares.

#![allow(clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use nucleosd::{App, router};
use std::sync::Arc;
use tower::ServiceExt;

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn an_unconfigured_daemon_describes_itself() {
    let app = router(Arc::new(App::unconfigured()));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/describe")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let described = body_json(response).await;
    assert_eq!(described["abi_version"], "v1");
    assert_eq!(
        described["models"].as_array().unwrap().len(),
        0,
        "no deployment, no models"
    );
}

#[tokio::test]
async fn starting_without_a_deployment_is_one_sentence_and_a_503() {
    let app = router(Arc::new(App::unconfigured()));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/sessions")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();

    // Not a 500 and not a panic three layers down: the daemon says exactly what is missing.
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let error = body_json(response).await;
    assert_eq!(error["code"], "unavailable");
    assert_eq!(error["retryable"], true);
    assert!(error["reason"].as_str().unwrap().contains("no deployment"));
    assert!(
        error.get("retry_after_millis").is_none(),
        "no hint means absent, not zero"
    );
}

#[tokio::test]
async fn an_unknown_route_is_a_404_without_a_body_contract() {
    let app = router(Arc::new(App::unconfigured()));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn every_row_of_the_table_is_a_route_the_router_serves() {
    // The table says what the document says; this proves the router agrees with it. A `Router`
    // cannot be asked what it serves, so the only way to check is to send the request: a wrong
    // verb answers 405, a missing path answers 404, and everything else means the row is real.
    for row in nucleosd::http::ROUTES {
        let path = row.meta.path.replace("{id}", "session-1");
        let mut request = Request::builder()
            .method(row.meta.method.to_uppercase().as_str())
            .uri(&path);
        let body = if row.meta.method == "get" {
            Body::empty()
        } else {
            request = request.header("content-type", "application/json");
            Body::from("{}")
        };

        let response = router(Arc::new(App::unconfigured()))
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();

        assert_ne!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{} {} is in the table but not in the router",
            row.meta.method.to_uppercase(),
            path
        );
        assert_ne!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{} {} documents a verb the router does not serve",
            row.meta.method.to_uppercase(),
            path
        );
    }
}

/// A daemon with a kernel that can actually run things: an in-memory store and no policy.
fn running() -> Arc<App> {
    use nucleos::adapter::{AllowAll, MemoryStore, Store};
    let kernel = nucleos::Kernel::builder()
        .store(Arc::new(MemoryStore::new()) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .build()
        .unwrap();
    Arc::new(App::new(
        Arc::new(kernel),
        nucleos::contract::Principal {
            tenant: "acme".to_owned(),
            subject: "alice".to_owned(),
        },
        Vec::new(),
    ))
}

#[tokio::test]
async fn a_process_runs_over_the_abi_and_its_output_is_in_the_stream() {
    let app = router(running());

    // Danger-full-access because no sandbox mechanism is wired up yet; a stricter policy is
    // refused rather than silently run unconfined, which the test below proves.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/processes")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"argv":["sh","-c","printf hi"],"sandbox":{"policy":{"kind":"danger_full_access"}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let started = body_json(response).await;
    let session = started["session"].as_str().unwrap().to_owned();
    assert!(started["pid"].as_u64().is_some());

    // Waiting is how a caller learns the exit code; the output arrives on the event stream.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/sessions/{session}/wait"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["exit_code"], 0);

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/sessions/{session}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let stream = String::from_utf8_lossy(&bytes);
    assert!(stream.contains("nucleos.v1.exec_started"), "{stream}");
    assert!(stream.contains("nucleos.v1.exec_output"), "{stream}");
    assert!(stream.contains("nucleos.v1.exec_exited"), "{stream}");
}

#[tokio::test]
async fn a_policy_without_a_mechanism_is_refused_over_the_abi() {
    let app = router(running());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/processes")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"argv":["sh","-c","printf hi"],"sandbox":{"policy":{"kind":"read_only"}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    // Running unconfined while the policy claims otherwise is worse than refusing.
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let error = body_json(response).await;
    assert!(
        error["reason"]
            .as_str()
            .unwrap()
            .contains("danger_full_access")
    );
}

#[tokio::test]
async fn the_list_shows_the_exec_sessions_and_says_how_they_ended() {
    let app = router(running());

    // Empty to begin with: an empty list is an answer, not an error.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/processes")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let listed = body_json(response).await;
    assert!(listed["processes"].as_array().unwrap().is_empty());

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/processes")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"argv":["sleep","30"],"sandbox":{"policy":{"kind":"danger_full_access"}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let session = body_json(response).await["session"]
        .as_str()
        .unwrap()
        .to_owned();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/processes")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let listed = body_json(response).await;
    let processes = listed["processes"].as_array().unwrap();
    assert_eq!(processes.len(), 1);
    assert_eq!(processes[0]["session"], session.as_str());
    //  is a tagged union, so the tag is the field to read.
    assert_eq!(processes[0]["process"]["status"]["status"], "running");
    assert!(processes[0]["process"]["status"]["pid"].as_u64().is_some());

    // Ending it does not remove it: the list is the store, and an exec session that ran is a fact
    // worth keeping. What changes is the status.
    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/sessions/{session}/signal"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/sessions/{session}/wait"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/processes")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let listed = body_json(response).await;
    let processes = listed["processes"].as_array().unwrap();
    assert_eq!(
        processes.len(),
        1,
        "an exec session that ran is still a fact"
    );
    assert_eq!(processes[0]["process"]["status"]["status"], "exited");
    // Killed by a signal: `-1`, because the platform gave no exit status. Not a failure of the
    // listing — a signalled process has no code.
    assert_eq!(processes[0]["process"]["status"]["output"]["exit_code"], -1);
}
