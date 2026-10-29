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
