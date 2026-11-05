//! The router, exercised without a socket.
//!
//! What matters here is the edge: an unconfigured daemon must describe itself and refuse to
//! pretend, and a failure must come back as the one error body every endpoint shares.

#![allow(clippy::unwrap_used, clippy::expect_used)]

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

/// A daemon with a kernel that can actually run things: the real store, in memory, and no policy.
fn running() -> Arc<App> {
    use nucleos::adapter::AllowAll;
    let kernel = nucleos::Kernel::builder()
        .model(
            "primary",
            Arc::new(nucleos_testkit::FakeModel::text(["unused"])),
        )
        .store(Arc::new(nucleos_sqlite::SqliteStore::in_memory().unwrap()))
        .policy(Arc::new(AllowAll))
        .routing(nucleos::contract::Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
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

/// A confining policy is enforced or refused, never quietly ignored.
///
/// The ABI has to answer one of the two on every platform: a `read_only` process that runs
/// unconfined would leave the caller believing the isolation was real.
#[tokio::test]
async fn a_confining_policy_is_enforced_or_refused_over_the_abi() {
    let app = router(running());
    let response = app
        .clone()
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

    // Enforced, or refused — and both have to say so. This split used to be
    // `#[cfg(not(target_os = "macos"))]`, which claimed every other platform refuses: Linux has
    // bubblewrap, and CI installs it.
    match response.status() {
        StatusCode::OK => {
            // Enforced. The evidence is in the session: a start event naming the mechanism that
            // carried the policy out. A policy silently ignored would run the command with none.
            let body = body_json(response).await;
            let session = body["session"]
                .as_str()
                .expect("the response names the session");
            let events = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(format!("/v1/sessions/{session}/events?from_seq=0"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(events.status(), StatusCode::OK);
            let body = String::from_utf8(
                axum::body::to_bytes(events.into_body(), 1 << 20)
                    .await
                    .unwrap()
                    .to_vec(),
            )
            .unwrap();
            assert!(body.contains("nucleos.v1.exec_started"), "{body}");
            assert!(
                body.contains(r#""mechanism":"bubblewrap""#)
                    || body.contains(r#""mechanism":"seatbelt""#),
                "the start names the mechanism: {body}"
            );
        }
        StatusCode::SERVICE_UNAVAILABLE => {
            // Refused. Running unconfined while the policy claims otherwise is worse than refusing.
            let error = body_json(response).await;
            let reason = error["reason"].as_str().unwrap_or_default();
            assert!(
                reason.contains("danger_full_access")
                    || reason.contains("bubblewrap")
                    || reason.contains("seatbelt"),
                "{reason}"
            );
        }
        other => panic!("neither enforced nor refused: {other}"),
    }
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

#[tokio::test]
async fn the_listing_filters_and_pages_over_the_abi() {
    let app = router(running());

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

    // A filter that matches nothing is an empty page.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/processes?phase=exited")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        body_json(response).await["processes"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/processes?phase=running&limit=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let listed = body_json(response).await;
    assert_eq!(listed["processes"].as_array().unwrap().len(), 1);
    assert_eq!(listed["processes"][0]["session"], session.as_str());
    // No second row, so there is no token to continue from.
    assert!(listed.get("next_page_token").is_none() || listed["next_page_token"].is_null());

    // A phase this build does not know is a client bug, not an empty page.
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/processes?phase=runningg")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error = body_json(response).await;
    assert!(
        error["reason"]
            .as_str()
            .unwrap()
            .contains("pending, running")
    );
}

#[tokio::test]
async fn a_source_is_added_by_request_and_its_tools_are_published() {
    let (app, key, key_id) = governable().await;
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../nucleos-mcp/tests/fixtures/echo_server.py"
    );

    let (status, added) = signed(
        &app,
        &key,
        &key_id,
        "POST",
        "/v1/sources",
        &format!(r#"{{"name":"echo","command":"python3","args":["-u","{script}"]}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    // Connected before it was acknowledged: a source that answers 200 and contributes nothing is a
    // change the operator cannot tell from a broken server.
    assert_eq!(added["tools"], serde_json::json!(["echo"]));

    // Publish happened as part of the request, so the tools are visible without a reload.
    let described = body_json(
        router(Arc::clone(&app))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/describe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(described["tools"], serde_json::json!(["echo"]));

    // And removing it publishes a generation without them.
    let (status, list) = signed(&app, &key, &key_id, "DELETE", "/v1/sources/echo", "").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["sources"], serde_json::json!([]));

    let described = body_json(
        router(Arc::clone(&app))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/describe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(described["tools"], serde_json::json!([]));
}

#[tokio::test]
async fn a_source_that_cannot_start_fails_the_request_rather_than_being_stored() {
    let (app, key, key_id) = governable().await;
    let (status, error) = signed(
        &app,
        &key,
        &key_id,
        "POST",
        "/v1/sources",
        r#"{"name":"broken","command":"definitely-not-a-real-command"}"#,
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{error}");
    // Stored anyway, it would fail on every boot instead of once, in front of the caller who asked.
    let (_, list) = signed(&app, &key, &key_id, "GET", "/v1/sources", "").await;
    assert_eq!(list["sources"], serde_json::json!([]));
}

#[tokio::test]
async fn a_reload_with_no_sources_says_what_to_do() {
    // Signed: a reload publishes a generation, so it is a configuration action and answers to the
    // administrative credential. An unsigned request never reaches the business logic.
    let (app, key, key_id) = governable().await;
    let (status, error) = signed(&app, &key, &key_id, "POST", "/v1/tools/reload", "").await;

    // Not a silent no-op: a reload that did nothing is a caller believing it deployed something.
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        error["reason"]
            .as_str()
            .unwrap()
            .contains("nucleos sources add"),
        "{error}"
    );
}

/// The invariant the authentication middleware relies on.
///
/// `authenticate` lets any `Authorization: Nucleos-Admin …` request reach its handler, on the argument
/// that the handler will refuse a signature it cannot verify. That argument only holds for handlers
/// that extract `Caller` or `Admin`. This walks **every published route** with a signature that
/// verifies nothing and asserts none of them answers success — so a route added later without a guard
/// fails here instead of shipping a public endpoint.
#[tokio::test]
async fn every_published_route_refuses_a_signature_that_verifies_nothing() {
    let (app, _key, _key_id) = governable().await;
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../../../openapi/v1/openapi.json")).unwrap();

    let mut open = Vec::new();
    for (path, methods) in schema["paths"].as_object().unwrap() {
        for method in methods.as_object().unwrap().keys() {
            let uri = path
                .replace("{id}", "x")
                .replace("{tenant}", "x")
                .replace("{name}", "x");
            let response = router(Arc::clone(&app))
                .oneshot(
                    Request::builder()
                        .method(method.to_uppercase().as_str())
                        .uri(&uri)
                        .header("authorization", "Nucleos-Admin k:1:n:AAAA")
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            // 2xx is the bug. 3xx would be a redirect out of the guard; both are failures. A `4xx`
            // from a body that does not parse is acceptable here — it proves nothing was *served*,
            // and the route-specific tests prove the guard itself.
            if response.status().is_success() || response.status().is_redirection() {
                open.push(format!("{method} {uri} -> {}", response.status()));
            }
        }
    }
    assert!(
        open.is_empty(),
        "these routes answered a signature that verifies nothing: {open:?}"
    );
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

/// A daemon that requires a bearer token, and what that token is.
///
/// Built the way a deployment builds it: `nucleos tokens add` puts a hash in the store, and the token
/// itself exists only here.
async fn guarded() -> (Arc<App>, String) {
    use nucleos::adapter::AllowAll;

    let system = Arc::new(nucleos_sqlite::SystemStore::in_memory().unwrap());
    let token = nucleos_auth::token::issue();
    system
        .add_token(&nucleos_sqlite::IssuedToken {
            id: token.id.clone(),
            tenant: "acme".to_owned(),
            subject: "alice".to_owned(),
            hash: token.hash.clone(),
            created_unix_millis: 1,
        })
        .await
        .unwrap();

    let kernel = nucleos::Kernel::builder()
        .store(Arc::new(nucleos_sqlite::SqliteStore::in_memory().unwrap()))
        .policy(Arc::new(AllowAll))
        .build()
        .unwrap();
    let app = Arc::new(
        App::new(
            Arc::new(kernel),
            nucleos::contract::Principal::default(),
            vec!["primary".to_owned()],
        )
        .with_system(
            Arc::clone(&system),
            std::path::PathBuf::from("/var/lib/nucleos"),
            Arc::new(nucleos::adapter::SystemClock),
        )
        .with_tokens(nucleosd::auth::TokenAuth::new(
            Arc::clone(&system),
            nucleos::contract::Principal::default(),
        )),
    );
    (app, token.token)
}

/// Two subjects, one tenant: the event stream must not be readable across them.
///
/// Session ids are chosen by callers and are short, so within one tenant every subject can guess every
/// other subject's. `events` is scoped by tenant — which is the wrong question for reading a
/// conversation — so the route loads the session first, and that is where ownership is checked.
#[tokio::test]
async fn a_subject_cannot_read_another_subjects_event_stream() {
    use nucleos::adapter::AllowAll;

    let system = Arc::new(nucleos_sqlite::SystemStore::in_memory().unwrap());
    let alice = nucleos_auth::token::issue();
    let bob = nucleos_auth::token::issue();
    for (token, subject) in [(&alice, "alice"), (&bob, "bob")] {
        system
            .add_token(&nucleos_sqlite::IssuedToken {
                id: token.id.clone(),
                tenant: "acme".to_owned(),
                subject: subject.to_owned(),
                hash: token.hash.clone(),
                created_unix_millis: 1,
            })
            .await
            .unwrap();
    }
    let kernel = Arc::new(
        nucleos::Kernel::builder()
            .model(
                "primary",
                Arc::new(nucleos_testkit::FakeModel::text(["unused"])),
            )
            .store(Arc::new(nucleos_sqlite::SqliteStore::in_memory().unwrap()))
            .policy(Arc::new(AllowAll))
            .routing(nucleos::contract::Routing {
                chain: vec!["primary".to_owned()],
                retries: 0,
            })
            .build()
            .unwrap(),
    );
    kernel
        .open(
            &nucleos::contract::Principal {
                tenant: "acme".to_owned(),
                subject: "alice".to_owned(),
            },
            nucleos::contract::SessionId::new("s1"),
            nucleos::kernel::SessionSpec {
                credit_tokens: Some(1_000),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let app = Arc::new(
        App::new(
            Arc::clone(&kernel),
            nucleos::contract::Principal::default(),
            vec!["primary".to_owned()],
        )
        .with_system(
            Arc::clone(&system),
            std::path::PathBuf::from("/var/lib/nucleos"),
            Arc::new(nucleos::adapter::SystemClock),
        )
        .with_tokens(nucleosd::auth::TokenAuth::new(
            Arc::clone(&system),
            nucleos::contract::Principal::default(),
        )),
    );

    let events = |token: String| {
        let app = Arc::clone(&app);
        async move {
            router(app)
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/v1/sessions/s1/events?from_seq=0")
                        .header("authorization", format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
        }
    };

    // The owner reads their own log.
    assert_eq!(events(alice.token.clone()).await, StatusCode::OK);
    // Another subject in the same tenant gets the same answer as a session that does not exist:
    // absent, not refused, because "this exists but is not yours" is a directory of identifiers.
    assert_eq!(events(bob.token.clone()).await, StatusCode::NOT_FOUND);
}

/// Every answer carries an id, and two answers do not share one.
///
/// The id is the only thing that connects "the request that failed at 14:02" to a line in the log and to
/// the counters. Without it, an operator has a timestamp and a guess; the request line has the session
/// and the tenant, but nothing to search by.
#[tokio::test]
async fn every_answer_carries_a_request_id() {
    let (app, _key, _key_id) = governable().await;
    let mut ids = Vec::new();
    for _ in 0..3 {
        let response = router(Arc::clone(&app))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/sessions/mine/events?from_seq=0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let id = response
            .headers()
            .get("nucleos-request-id")
            .and_then(|value| value.to_str().ok())
            .expect("every response names the request")
            .to_owned();
        ids.push(id);
    }
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 3, "ids are per request, not per route");
}

/// Removing a session: only a closed one, and only its owner.
///
/// Deletion is the one operation with no undo, so the two refusals come before anything is removed: an
/// open session is still somebody's conversation (`409`), and another subject's session does not exist
/// for you (`404`).
#[tokio::test]
async fn a_session_is_deleted_only_when_closed_and_owned() {
    let (app, _key, _key_id) = governable().await;
    let token = issue_token_for(&app, "acme", "alice").await;

    let (status, _) = bearer(
        &app,
        &token,
        "POST",
        "/v1/sessions",
        r#"{"session":"s1","credit_tokens":1000}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Open: refused, and the reason says what to do first.
    let (status, refused) = bearer(&app, &token, "DELETE", "/v1/sessions/s1", "").await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(
        refused["reason"].as_str().unwrap().contains("close"),
        "{refused}"
    );

    let (status, closed) = bearer(&app, &token, "POST", "/v1/sessions/s1/close", "{}").await;
    assert_eq!(status, StatusCode::OK, "{closed}");

    let (status, deleted) = bearer(&app, &token, "DELETE", "/v1/sessions/s1", "").await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["deleted"], true);

    // Gone from every reading: the view, the log, and the listing.
    let (status, missing) = bearer(&app, &token, "GET", "/v1/sessions/s1", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
    let (status, events) =
        bearer(&app, &token, "GET", "/v1/sessions/s1/events?from_seq=0", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{events}");
    let (status, listed) = bearer(&app, &token, "GET", "/v1/sessions", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["sessions"], serde_json::json!([]), "{listed}");

    // And deleting it again is a 404 rather than a success nobody can tell from the first.
    let (status, _) = bearer(&app, &token, "DELETE", "/v1/sessions/s1", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A route that matched nothing is one counter, not one per guess.
///
/// The metrics layer runs outside authentication, so an unauthenticated caller reaches it — and a key
/// derived from the raw path turns "try a thousand URLs" into a thousand permanent map entries.
#[tokio::test]
async fn paths_that_match_nothing_share_one_counter() {
    let (app, _key, _key_id) = governable().await;
    for guess in ["nope", "worse", "still-nothing"] {
        let _ = router(Arc::clone(&app))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/v1/{guess}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
    }

    let counters = app.metrics().snapshot().requests;
    assert!(
        counters.contains_key("GET unmatched"),
        "unmatched paths are bucketed: {counters:?}"
    );
    assert!(
        counters
            .keys()
            .all(|key| !key.contains("nope") && !key.contains("worse")),
        "no key is derived from the path: {counters:?}"
    );
    assert_eq!(counters.len(), 1, "three guesses, one key: {counters:?}");
}

#[tokio::test]
async fn a_configured_token_is_required_on_every_route() {
    let (app, token) = guarded().await;

    // `describe` too: an exempt health endpoint would tell an unauthenticated caller what the
    // deployment has.
    for uri in ["/v1/describe", "/v1/sessions/s1/events"] {
        let response = router(Arc::clone(&app))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        let error = body_json(response).await;
        assert_eq!(error["code"], "denied", "{error}");
        assert!(error["reason"].as_str().unwrap().contains("bearer"));
    }

    let allowed = router(Arc::clone(&app))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/describe")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::OK);

    let refused = router(app)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/describe")
                .header("authorization", "Bearer nu_not-a-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Telemetry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn metrics_count_what_an_operator_asks_about() {
    let (app, token) = guarded().await;
    // Metrics is an administrator's view: the fixture's bearer token is not enough for it.
    let admin_key = nucleos_auth::KeyPair::generate();
    let public = admin_key.public("acme", "admin");
    let key_id = public.id.clone();
    app.system()
        .unwrap()
        .set(
            nucleosd::admin::ADMIN_KEYS_KEY,
            &serde_json::to_string(&vec![public]).unwrap(),
        )
        .await
        .unwrap();

    // One allowed request, one refused, one reload attempt with no source.
    let _ = router(Arc::clone(&app))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/describe")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let _ = router(Arc::clone(&app))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/describe")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let (status, body) = signed(&app, &admin_key, &key_id, "GET", "/v1/metrics", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Keyed by route template, so a thousand sessions are one counter rather than a thousand.
    assert_eq!(body["requests"]["GET /v1/describe"], 2);
    assert_eq!(body["unauthorized"], 1, "a refusal is still a request");
    // The request that produced this snapshot is counted after the body is built, so it cannot
    // include itself — which is the correct behaviour and the reason it is not asserted.
    assert!(body["uptime_millis"].is_u64() || body["uptime_millis"].is_f64());
}

// ---------------------------------------------------------------------------
// Tenants
//
// The store keys sessions by id alone, so "whose session is this" is a question the daemon has to
// answer. These tests are the answer: an id belongs to the tenant that created it.
// ---------------------------------------------------------------------------

async fn multi_tenant() -> (Arc<App>, String, String) {
    use nucleos::adapter::AllowAll;

    // Tokens are issued the way a deployment issues them: into the store, hashed, with the token
    // itself only ever leaving this function as a value the test sends.
    let system = Arc::new(nucleos_sqlite::SystemStore::in_memory().unwrap());
    let mut issued = Vec::new();
    for (tenant, subject) in [("acme", "alice"), ("other", "bob")] {
        let token = nucleos_auth::token::issue();
        system
            .add_token(&nucleos_sqlite::IssuedToken {
                id: token.id.clone(),
                tenant: tenant.to_owned(),
                subject: subject.to_owned(),
                hash: token.hash.clone(),
                created_unix_millis: 1,
            })
            .await
            .unwrap();
        issued.push(token.token);
    }

    let kernel = nucleos::Kernel::builder()
        .model(
            "primary",
            Arc::new(nucleos_testkit::FakeModel::text(["unused"])),
        )
        .store(Arc::new(nucleos_sqlite::SqliteStore::in_memory().unwrap()))
        .policy(Arc::new(AllowAll))
        .routing(nucleos::contract::Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap();

    let app = Arc::new(
        App::new(
            Arc::new(kernel),
            nucleos::contract::Principal::default(),
            Vec::new(),
        )
        .with_system(
            Arc::clone(&system),
            std::path::PathBuf::from("/var/lib/nucleos"),
            Arc::new(nucleos::adapter::SystemClock),
        )
        .with_tokens(nucleosd::auth::TokenAuth::new(
            Arc::clone(&system),
            nucleos::contract::Principal::default(),
        )),
    );
    let mut tokens = issued.into_iter();
    (app, tokens.next().unwrap(), tokens.next().unwrap())
}

async fn request(
    app: &Arc<App>,
    method: &str,
    uri: &str,
    token: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let response = router(Arc::clone(app))
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (status, body_json(response).await)
}

#[tokio::test]
async fn a_session_belongs_to_the_tenant_that_created_it() {
    let (app, acme, other) = multi_tenant().await;

    let (created, _) = request(&app, "POST", "/v1/sessions", &acme, r#"{"session":"s1"}"#).await;
    assert_eq!(created, StatusCode::OK);

    // The owner can read it.
    let (mine, _) = request(&app, "GET", "/v1/sessions/s1", &acme, "").await;
    assert_eq!(mine, StatusCode::OK);

    // Another tenant gets the same answer as for an id that never existed. A 403 would tell them
    // which identifiers their neighbours are using.
    let (theirs, error) = request(&app, "GET", "/v1/sessions/s1", &other, "").await;
    assert_eq!(theirs, StatusCode::NOT_FOUND);
    assert_eq!(error["code"], "not_found");
}

#[tokio::test]
async fn the_same_identifier_in_two_tenants_is_two_sessions() {
    let (app, acme, other) = multi_tenant().await;

    // Callers choose their ids, so "s1" from two tenants is ordinary. The store's key is
    // (tenant, id), so both succeed and neither sees the other's history.
    let (first, _) = request(&app, "POST", "/v1/sessions", &acme, r#"{"session":"s1"}"#).await;
    let (second, _) = request(&app, "POST", "/v1/sessions", &other, r#"{"session":"s1"}"#).await;
    assert_eq!(first, StatusCode::OK);
    assert_eq!(second, StatusCode::OK, "distinct keys, not a collision");

    // Each tenant reads its own stream. The events route answers SSE, so this asks for the status
    // rather than trying to parse a body.
    for token in [&acme, &other] {
        let response = router(Arc::clone(&app))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/sessions/s1/events")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{token}");
    }
}

#[tokio::test]
async fn two_tenants_each_get_their_own_session() {
    let (app, acme, other) = multi_tenant().await;

    request(&app, "POST", "/v1/sessions", &acme, r#"{"session":"a1"}"#).await;
    request(&app, "POST", "/v1/sessions", &other, r#"{"session":"b1"}"#).await;

    // Different ids, so no collision: each caller sees only its own.
    assert_eq!(
        request(&app, "GET", "/v1/sessions/a1", &acme, "").await.0,
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "GET", "/v1/sessions/b1", &other, "").await.0,
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "GET", "/v1/sessions/b1", &acme, "").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_token_that_is_not_in_the_document_is_refused() {
    let (app, _acme, _other) = multi_tenant().await;
    let (status, error) = request(&app, "GET", "/v1/describe", "nobody-token", "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error["code"], "denied");
}

#[tokio::test]
async fn the_session_view_reports_a_turn_that_was_cut_short() {
    // The store is held here so the test can write what a crash leaves behind: the deltas of a turn
    // that never completed.
    let store = Arc::new(nucleos_sqlite::SqliteStore::in_memory().unwrap());
    let kernel = nucleos::Kernel::builder()
        .model(
            "primary",
            Arc::new(nucleos_testkit::FakeModel::text(["unused"])),
        )
        .store(Arc::clone(&store) as Arc<dyn nucleos::adapter::Store>)
        .policy(Arc::new(nucleos::adapter::AllowAll))
        .routing(nucleos::contract::Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap();
    let app = Arc::new(App::new(
        Arc::new(kernel),
        nucleos::contract::Principal {
            tenant: "acme".to_owned(),
            subject: "alice".to_owned(),
        },
        Vec::new(),
    ));

    request(&app, "POST", "/v1/sessions", "", r#"{"session":"s1"}"#).await;
    use nucleos::adapter::Store as _;

    let key = nucleos::adapter::SessionKey::new("acme", nucleos::contract::SessionId::new("s1"));
    let head = store.events_since(&key, 0).await.unwrap().len() as u64;
    let events: Vec<nucleos::contract::Event> = [
        nucleos::contract::EventKind::TextDelta {
            part_index: 0,
            text: "check the config, then run ".to_owned(),
        },
        nucleos::contract::EventKind::TextDelta {
            part_index: 0,
            text: "migrate".to_owned(),
        },
    ]
    .into_iter()
    .enumerate()
    .map(|(offset, kind)| nucleos::contract::Event {
        seq: head + offset as u64,
        session: nucleos::contract::SessionId::new("s1"),
        at_unix_millis: 1,
        schema_version: nucleos::contract::Event::SCHEMA_VERSION,
        kind,
        meta: Default::default(),
    })
    .collect();
    store.append(&key, &events).await.unwrap();

    let (status, view) = request(&app, "GET", "/v1/sessions/s1", "", "").await;
    assert_eq!(status, StatusCode::OK);
    // The caller no longer has to walk the event stream to find out what was said before the crash.
    assert_eq!(
        view["partial"]["text"],
        "check the config, then run migrate"
    );
    assert!(
        view["partial"].get("unfinished_call").is_none(),
        "it stopped in the text, not inside a call: {view}"
    );
}

#[tokio::test]
async fn a_finished_session_has_no_partial_in_its_view() {
    let app = running();
    request(&app, "POST", "/v1/sessions", "", r#"{"session":"s1"}"#).await;

    let (status, view) = request(&app, "GET", "/v1/sessions/s1", "", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        view.get("partial").is_none(),
        "absent, not empty: a session that was never interrupted says nothing about it. {view}"
    );
}

#[tokio::test]
async fn the_daemon_reads_the_policy_init_wrote_instead_of_a_file() {
    // The pair of constants is the risk: two crates, one key name, no shared import. This is the test
    // that makes the duplication safe — if either side renames it, this fails.
    assert_eq!(
        nucleosd::http::POLICY_KEY,
        nucleos_cli::init::POLICY_KEY,
        "the daemon must read the row `init` writes"
    );
}

#[tokio::test]
async fn the_config_endpoint_needs_a_signature_and_says_so_with_401() {
    // A built deployment: an admin key exists, so the endpoint is reachable in principle and the
    // refusal is about *this* request rather than about the daemon being uninitialised.
    let app = signed_deployment().await;

    let response = router(Arc::clone(&app))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // 401, not 403: a missing credential is "who are you", a denied policy verdict is "not you".
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let error = body_json(response).await;
    assert!(
        error["reason"].as_str().unwrap().contains("Nucleos-Admin"),
        "{error}"
    );
}

/// A daemon with a system store and one enrolled administrator key.
async fn signed_deployment() -> Arc<App> {
    use nucleos::adapter::AllowAll;
    let system = Arc::new(nucleos_sqlite::SystemStore::in_memory().unwrap());
    let key = nucleos_auth::KeyPair::generate();
    system
        .set(
            nucleosd::admin::ADMIN_KEYS_KEY,
            &serde_json::to_string(&vec![key.public("acme", "alice")]).unwrap(),
        )
        .await
        .unwrap();

    let kernel = nucleos::Kernel::builder()
        .store(Arc::new(nucleos_sqlite::SqliteStore::in_memory().unwrap()))
        .policy(Arc::new(AllowAll))
        .build()
        .unwrap();

    Arc::new(
        App::new(
            Arc::new(kernel),
            nucleos::contract::Principal::default(),
            Vec::new(),
        )
        .with_system(
            system,
            std::path::PathBuf::from("/var/lib/nucleos"),
            Arc::new(nucleos::adapter::SystemClock),
        ),
    )
}

#[tokio::test]
async fn a_signed_config_request_answers_with_what_the_daemon_is_running() {
    let system = Arc::new(nucleos_sqlite::SystemStore::in_memory().unwrap());
    let key = nucleos_auth::KeyPair::generate();
    let public = key.public("acme", "alice");
    system
        .set(
            nucleosd::admin::ADMIN_KEYS_KEY,
            &serde_json::to_string(&vec![public.clone()]).unwrap(),
        )
        .await
        .unwrap();
    system
        .set(
            nucleosd::http::POLICY_KEY,
            r#"{"default":"deny","rules":[]}"#,
        )
        .await
        .unwrap();

    use nucleos::adapter::AllowAll;
    let kernel = nucleos::Kernel::builder()
        .store(Arc::new(nucleos_sqlite::SqliteStore::in_memory().unwrap()))
        .policy(Arc::new(AllowAll))
        .build()
        .unwrap();
    let app = Arc::new(
        App::new(
            Arc::new(kernel),
            nucleos::contract::Principal::default(),
            Vec::new(),
        )
        .with_system(
            system,
            std::path::PathBuf::from("/var/lib/nucleos"),
            Arc::new(nucleos::adapter::SystemClock),
        ),
    );

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let authorization = key.sign_request(&public.id, "GET", "/v1/config", b"", now);

    let response = router(Arc::clone(&app))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/config")
                .header("authorization", authorization)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let config = body_json(response).await;
    assert_eq!(config["root"], "/var/lib/nucleos");
    assert_eq!(config["admin_keys"][0]["id"], public.id.as_str());
    assert_eq!(config["admin_keys"][0]["subject"], "alice");
    assert_eq!(config["policy_json"], r#"{"default":"deny","rules":[]}"#);
    assert_eq!(config["signed_body_bytes"], 0, "a read signs an empty body");
}

// ---------------------------------------------------------------------------
// Configuration changes
// ---------------------------------------------------------------------------

/// Who may change what, in the fixture: one administrator, every subject, one tenant.
const GOVERNED_BY: &str = r#"{
  "default": "deny",
  "rules": [
    {"decision": "allow", "config": "*", "scope": "system", "tenant": "acme", "subject": "admin"},
    {"decision": "allow", "config": "*", "scope": "own", "tenant": "acme", "subject": "admin"},
    {"decision": "allow", "sandbox": "*", "network": true,
     "reason": "the fixture's callers may open any session; a real deployment would not be this broad"}
  ]
}"#;

/// A deployment that enforces the policy it stores, with one administrator enrolled.
async fn governable() -> (Arc<App>, nucleos_auth::KeyPair, String) {
    governable_with(None).await
}

/// The same fixture, with a seal key when a test needs one.
async fn governable_with(
    seal: Option<nucleos_auth::SealKey>,
) -> (Arc<App>, nucleos_auth::KeyPair, String) {
    governable_by(GOVERNED_BY, seal).await
}

/// The same fixture, with a policy of the test's choosing.
async fn governable_by(
    governed_by: &str,
    seal: Option<nucleos_auth::SealKey>,
) -> (Arc<App>, nucleos_auth::KeyPair, String) {
    let system = Arc::new(nucleos_sqlite::SystemStore::in_memory().unwrap());
    let key = nucleos_auth::KeyPair::generate();
    let public = key.public("acme", "admin");
    system
        .set(
            nucleosd::admin::ADMIN_KEYS_KEY,
            &serde_json::to_string(&vec![public.clone()]).unwrap(),
        )
        .await
        .unwrap();
    // The store holds the same policy the kernel starts with. They must agree: a tenant write rebuilds
    // the composed policy **from the store**, so a fixture whose store says something different would
    // change the rules silently on the first change — which is a bug in the fixture and, in a daemon,
    // the reason `load` never takes a policy from anywhere else.
    system
        .set(nucleosd::http::POLICY_KEY, governed_by)
        .await
        .unwrap();

    // The kernel enforces the same policy that is stored, as the daemon does at start-up: a fixture
    // with a permissive gate would test the endpoints without testing the thing in front of them.
    let policy =
        nucleos_policy::Ruleset::new(nucleos_policy::Document::parse(governed_by).unwrap())
            .unwrap();
    // The store is kept: `GET /v1/sessions` reads it directly, because a page of a hundred sessions
    // must not load a hundred conversations.
    let sessions: Arc<dyn nucleos::adapter::Store> =
        Arc::new(nucleos_sqlite::SqliteStore::in_memory().unwrap());
    let kernel = nucleos::Kernel::builder()
        // A model, so the fixture can open an agent session: `GET /v1/sessions` is only interesting
        // when there is something to list.
        .model(
            "primary",
            Arc::new(nucleos_testkit::FakeModel::text(["unused"])),
        )
        .routing(nucleos::contract::Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .store(Arc::clone(&sessions))
        .policy(Arc::new(policy))
        .build()
        .unwrap();
    let app = Arc::new(
        App::new(
            Arc::new(kernel),
            nucleos::contract::Principal::default(),
            Vec::new(),
        )
        .with_store(sessions)
        .with_system(
            system,
            std::path::PathBuf::from("/var/lib/nucleos"),
            Arc::new(nucleos::adapter::SystemClock),
        )
        .with_seal(seal),
    );
    let id = public.id.clone();
    (app, key, id)
}

async fn signed(
    app: &Arc<App>,
    key: &nucleos_auth::KeyPair,
    key_id: &str,
    method: &str,
    path: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let authorization = key.sign_request(key_id, method, path, body.as_bytes(), now);
    let response = router(Arc::clone(app))
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", authorization)
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (status, body_json(response).await)
}

/// A bearer request, for the routes that take a tenant rather than an administrator.
async fn bearer(
    app: &Arc<App>,
    token: &str,
    method: &str,
    path: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let response = router(Arc::clone(app))
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (status, body_json(response).await)
}

/// Issue a tenant token into the running fixture's store.
async fn issue_token_for(app: &Arc<App>, tenant: &str, subject: &str) -> String {
    let token = nucleos_auth::token::issue();
    app.system()
        .expect("the fixture has a system store")
        .add_token(&nucleos_sqlite::IssuedToken {
            id: token.id.clone(),
            tenant: tenant.to_owned(),
            subject: subject.to_owned(),
            hash: token.hash.clone(),
            created_unix_millis: 1,
        })
        .await
        .unwrap();
    token.token
}

/// The same signed request, with extra headers.
async fn signed_with(
    app: &Arc<App>,
    key: &nucleos_auth::KeyPair,
    key_id: &str,
    method: &str,
    path: &str,
    body: &str,
    extra: &[(&str, &str)],
) -> (StatusCode, serde_json::Value) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let authorization = key.sign_request(key_id, method, path, body.as_bytes(), now);
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", authorization)
        .header("content-type", "application/json");
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    let response = router(Arc::clone(app))
        .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    (status, body_json(response).await)
}

#[tokio::test]
async fn the_configuration_is_one_document_that_can_be_applied_back() {
    let (app, key, key_id) = governable().await;

    let (status, before) = signed(&app, &key, &key_id, "GET", "/v1/config", "").await;
    assert_eq!(status, StatusCode::OK, "{before}");
    let revision = before["revision"].as_u64().expect("a revision");

    // The document: the whole configuration, not a summary of it.
    let document = r#"{
        "document_version": 1,
        "policy": "{\"default\":\"deny\",\"rules\":[]}",
        "models": [],
        "sources": [],
        "tenants": { "acme": { "limits": { "tenant": "acme", "tools": ["read_file"] } } }
    }"#;
    let (status, applied) = signed(&app, &key, &key_id, "PUT", "/v1/config", document).await;
    assert_eq!(status, StatusCode::OK, "{applied}");
    assert_eq!(applied["revision"], revision + 1, "{applied}");

    // Read back: what was applied is what the document says.
    let (status, after) = signed(&app, &key, &key_id, "GET", "/v1/config", "").await;
    assert_eq!(status, StatusCode::OK, "{after}");
    assert_eq!(after["revision"], revision + 1);
    assert_eq!(
        after["tenants"]["acme"]["limits"]["tools"],
        serde_json::json!(["read_file"]),
        "{after}"
    );

    // And the history names the key that signed it — the audit question the row exists for.
    let (status, history) = signed(&app, &key, &key_id, "GET", "/v1/config/history", "").await;
    assert_eq!(status, StatusCode::OK, "{history}");
    let rows = history["changes"].as_array().unwrap();
    let row = rows
        .iter()
        .find(|row| row["key"] == "deployment")
        .expect("the apply is recorded");
    assert_eq!(row["key_id"], key_id, "{row}");
    assert_eq!(row["subject"], "admin", "{row}");
    assert_eq!(row["tenant"], "acme", "{row}");
}

#[tokio::test]
async fn a_document_written_against_an_older_revision_is_refused() {
    let (app, key, key_id) = governable().await;
    let (_, before) = signed(&app, &key, &key_id, "GET", "/v1/config", "").await;
    let stale = before["revision"].as_u64().unwrap();

    let document = r#"{"document_version":1,"policy":"{\"default\":\"deny\",\"rules\":[]}",
                       "models":[],"sources":[],"tenants":{}}"#;
    let first = format!("\"{stale}\"");
    let (status, applied) = signed_with(
        &app,
        &key,
        &key_id,
        "PUT",
        "/v1/config",
        document,
        &[("if-match", &first)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the first apply wins: {applied}");

    // The same document again, written against the revision it has just superseded.
    let (status, error) = signed_with(
        &app,
        &key,
        &key_id,
        "PUT",
        "/v1/config",
        document,
        &[("if-match", &first)],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["code"], "conflict", "{error}");
}

/// Removing a model a session's route names is allowed, and the session fails by *saying which*.
///
/// A document is the deployment's truth: a model it no longer lists is gone, and a session that was
/// routed to it cannot silently keep using an adapter the operator removed. What must not happen is an
/// unnamed failure or a session stuck in a state nobody can leave — the error names the model and lists
/// the ones that exist, and applying the model back makes the same session work again.
#[tokio::test]
async fn removing_a_model_fails_the_next_turn_by_name_and_is_recoverable() {
    let (app, key, key_id) = governable().await;

    let token = issue_token_for(&app, "acme", "alice").await;
    let (status, opened) = bearer(
        &app,
        &token,
        "POST",
        "/v1/sessions",
        r#"{"session":"s1","credit_tokens":1000}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{opened}");

    let without = r#"{
        "document_version": 1,
        "policy": "{\"default\":\"allow\",\"rules\":[]}",
        "models": [], "sources": [], "tenants": {}
    }"#;
    let (status, applied) = signed(&app, &key, &key_id, "PUT", "/v1/config", without).await;
    assert_eq!(status, StatusCode::OK, "{applied}");

    let (status, failed) = bearer(
        &app,
        &token,
        "POST",
        "/v1/sessions/s1/inference",
        r#"{"messages":[]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{failed}");
    let reason = failed["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("primary"),
        "the error names the model: {reason}"
    );

    // The session is still there: a deployment change does not end conversations.
    let (status, view) = bearer(&app, &token, "GET", "/v1/sessions/s1", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["state"], "open");

    // Putting the model back makes the same session work again.
    let back = r#"{
        "document_version": 1,
        "policy": "{\"default\":\"allow\",\"rules\":[]}",
        "models": [{"name":"primary","url":"https://api.example.invalid/v1/responses","model":"m","context_window":128000}],
        "sources": [], "tenants": {}
    }"#;
    let (status, applied) = signed(&app, &key, &key_id, "PUT", "/v1/config", back).await;
    assert_eq!(status, StatusCode::OK, "{applied}");

    let (status, answered) = bearer(
        &app,
        &token,
        "POST",
        "/v1/sessions/s1/inference",
        r#"{"messages":[]}"#,
    )
    .await;
    // The adapter points at an unreachable host, so the turn fails — but it fails *reaching* the model
    // (503, unavailable) rather than failing to find it (404, not found), which is the difference this
    // test is about: the same session recovers when the deployment does.
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{answered}");
}

/// Export, then apply what was exported: the deployment does not change, except its revision.
///
/// This is the property that makes the document a backup rather than a screenshot. If a round trip
/// reordered, normalised or dropped anything, restoring it would be a change nobody asked for.
#[tokio::test]
async fn an_export_applied_back_is_a_no_op_except_the_revision() {
    let (app, key, key_id) = governable().await;

    let document = r#"{
        "document_version": 1,
        "policy": "{\"default\":\"deny\",\"rules\":[{\"decision\":\"allow\",\"model\":\"*\",\"tenant\":\"acme\",\"subject\":\"admin\"}]}",
        "models": [{"name":"primary","url":"https://api.example.invalid/v1/responses","model":"m","context_window":128000}],
        "sources": [],
        "tenants": {"acme": {"limits": {"tenant":"acme","models":["primary"]}}}
    }"#;
    let (status, first) = signed(&app, &key, &key_id, "PUT", "/v1/config", document).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let revision = first["revision"].as_u64().unwrap();

    let (status, exported) = signed(&app, &key, &key_id, "GET", "/v1/config", "").await;
    assert_eq!(status, StatusCode::OK);

    // Exactly what `GET` produced, sent back. `revision` is the daemon's to write, so it goes along
    // and is ignored.
    let body = serde_json::json!({
        "document_version": exported["document_version"],
        "revision": exported["revision"],
        "policy": exported["policy_json"],
        "models": exported["models"],
        "sources": exported["sources"],
        "tenants": exported["tenants"],
    })
    .to_string();
    let (status, second) = signed(&app, &key, &key_id, "PUT", "/v1/config", &body).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["revision"], revision + 1);

    let (status, after) = signed(&app, &key, &key_id, "GET", "/v1/config", "").await;
    assert_eq!(status, StatusCode::OK);
    for field in ["policy_json", "models", "sources", "tenants"] {
        assert_eq!(
            after[field], exported[field],
            "{field} changed by a round trip: {} vs {}",
            after[field], exported[field]
        );
    }
}

#[tokio::test]
async fn a_policy_change_is_applied_and_recorded() {
    let (app, key, key_id) = governable().await;
    let document =
        r#"{"default":"deny","rules":[{"decision":"allow","config":"*","scope":"system"}]}"#;

    let (status, reply) = signed(&app, &key, &key_id, "PUT", "/v1/policy", document).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["changed"], "policy");
    assert_eq!(reply["rules"], 1);

    // The new policy is what the endpoint reports afterwards, and what the store keeps.
    let (status, config) = signed(&app, &key, &key_id, "GET", "/v1/config", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(config["policy_json"], document);
}

#[tokio::test]
async fn an_invalid_policy_is_refused_before_it_is_stored() {
    let (app, key, key_id) = governable().await;
    let (status, error) = signed(
        &app,
        &key,
        &key_id,
        "PUT",
        "/v1/policy",
        r#"{"rules":[{"decision":"allow"}]}"#,
    )
    .await;
    // A rule that names nothing is a caller error, not a policy.
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    assert!(
        error["reason"]
            .as_str()
            .unwrap()
            .contains("matches nothing"),
        "{error}"
    );
}

#[tokio::test]
async fn a_model_can_be_added_listed_and_removed() {
    let (app, key, key_id) = governable().await;
    let model = r#"{"name":"primary","url":"https://example.invalid/v1/responses","model":"m"}"#;

    let (status, added) = signed(&app, &key, &key_id, "POST", "/v1/models", model).await;
    assert_eq!(status, StatusCode::OK, "{added}");
    assert_eq!(added["name"], "primary");

    let (status, list) = signed(&app, &key, &key_id, "GET", "/v1/models", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["models"][0]["name"], "primary");

    let (status, list) = signed(&app, &key, &key_id, "DELETE", "/v1/models/primary", "").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert!(list["models"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_change_the_policy_does_not_allow_is_403_not_401() {
    // A key that is valid, and an administrator the policy does not let change the policy. The
    // signature is fine; the authority is not — which is a different status code from a missing
    // credential, and the whole reason `check_config` exists separately from the signature check.
    let (app, _, _) = governable().await;
    let outsider = nucleos_auth::KeyPair::generate();
    let public = outsider.public("acme", "intern");
    app.system()
        .unwrap()
        .set(
            nucleosd::admin::ADMIN_KEYS_KEY,
            &serde_json::to_string(&vec![public.clone()]).unwrap(),
        )
        .await
        .unwrap();

    let (status, error) = signed(
        &app,
        &outsider,
        &public.id,
        "PUT",
        "/v1/policy",
        r#"{"default":"allow","rules":[]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert!(
        error["reason"].as_str().unwrap().contains("configuration"),
        "{error}"
    );
}

#[tokio::test]
async fn a_new_policy_governs_the_next_request() {
    let (app, key, key_id) = governable().await;

    // Replacing the policy is allowed while the old one allows it…
    let (status, _) = signed(
        &app,
        &key,
        &key_id,
        "PUT",
        "/v1/policy",
        r#"{"default":"deny","rules":[]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // …and the new one takes effect on the very next request, not at the next restart. The same
    // administrator, the same key, now refuses itself: that is what "read per call" means.
    let (status, error) = signed(
        &app,
        &key,
        &key_id,
        "POST",
        "/v1/models",
        r#"{"name":"m","url":"https://example.invalid/v1/responses","model":"x"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
}

#[tokio::test]
async fn every_change_lands_in_the_deployment_history() {
    let (app, key, key_id) = governable().await;
    // A policy that keeps this administrator able to change models, so both changes below succeed.
    signed(&app, &key, &key_id, "PUT", "/v1/policy", GOVERNED_BY).await;
    signed(
        &app,
        &key,
        &key_id,
        "POST",
        "/v1/models",
        r#"{"name":"m","url":"https://example.invalid/v1/responses","model":"x"}"#,
    )
    .await;

    let history = app.system().unwrap().history(10).await.unwrap();
    let keys: Vec<&str> = history.iter().map(|change| change.key.as_str()).collect();
    // Newest first: `limit` means "how many do I want to know about", and the answer to that is the
    // recent ones — not the three oldest changes the deployment ever had.
    assert_eq!(keys, ["models", "policy"], "{history:?}");
    assert!(
        history.iter().all(|change| change.subject == "admin"),
        "every row names who made the change"
    );
    // And by which key: an audit that cannot say that proves authorisation without identity.
    assert!(
        history.iter().all(|change| change.key_id == key_id),
        "every row names the key that signed: {history:?}"
    );
}

#[tokio::test]
async fn a_model_added_by_request_is_visible_without_a_restart() {
    let (app, key, key_id) = governable().await;

    let (status, config) = signed(&app, &key, &key_id, "GET", "/v1/config", "").await;
    assert_eq!(status, StatusCode::OK, "{config}");

    signed(
        &app,
        &key,
        &key_id,
        "POST",
        "/v1/models",
        r#"{"name":"primary","url":"https://example.invalid/v1/responses","model":"m"}"#,
    )
    .await;

    // `describe` is what a caller reads before opening a session. Two snapshots used to be frozen at
    // start-up — the advertised list and the default route — so a model added by request worked for
    // nobody but the session that named it.
    let described = body_json(
        router(Arc::clone(&app))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/describe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(described["models"], serde_json::json!(["primary"]));
}

#[tokio::test]
async fn a_tenant_policy_is_stored_and_reported() {
    let (app, key, key_id) = governable().await;

    let (status, reply) = signed(
        &app,
        &key,
        &key_id,
        "PUT",
        "/v1/tenants/acme/policy",
        r#"{"default":"deny","rules":[]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["changed"], "policy/acme");

    // "What is this daemon enforcing" is incomplete without knowing which tenants said something
    // stricter — and the list is derived, not remembered.
    let (_, config) = signed(&app, &key, &key_id, "GET", "/v1/config", "").await;
    assert_eq!(config["tenant_policies"], serde_json::json!(["acme"]));
}

#[tokio::test]
async fn a_tenant_may_change_only_its_own_configuration() {
    // `scope: "own"` compares against the caller, which is what lets a deployment delegate
    // self-service without one tenant editing another's settings.
    let (app, key, key_id) = governable().await;

    let (allowed, _) = signed(
        &app,
        &key,
        &key_id,
        "PUT",
        "/v1/tenants/acme/policy",
        r#"{"default":"deny","rules":[]}"#,
    )
    .await;
    assert_eq!(allowed, StatusCode::OK);

    let (refused, error) = signed(
        &app,
        &key,
        &key_id,
        "PUT",
        "/v1/tenants/other/policy",
        r#"{"default":"allow","rules":[]}"#,
    )
    .await;
    assert_eq!(refused, StatusCode::FORBIDDEN, "{error}");
}

#[tokio::test]
async fn a_tenant_policy_that_allows_everything_cannot_widen_the_deployment() {
    // The composition rule, end to end: the tenant's document is a second answer, and the stricter
    // answer is the answer. The deployment is made to refuse configuration outright; the tenant then
    // writes an "allow everything" policy of its own, and the refusal still stands.
    let (app, key, key_id) = governable().await;

    signed(
        &app,
        &key,
        &key_id,
        "PUT",
        "/v1/tenants/acme/policy",
        r#"{"default":"allow","rules":[]}"#,
    )
    .await;

    // Narrow the deployment's policy to models only — allowed while the old one permits it.
    let (status, _) = signed(
        &app,
        &key,
        &key_id,
        "PUT",
        "/v1/policy",
        r#"{"default":"deny","rules":[{"decision":"allow","config":"models","scope":"system",
            "tenant":"acme","subject":"admin"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The tenant's "allow everything" loses to the deployment's refusal.
    let (status, error) = signed(
        &app,
        &key,
        &key_id,
        "PUT",
        "/v1/tenants/acme/policy",
        r#"{"default":"allow","rules":[]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
}

#[tokio::test]
async fn tenant_limits_are_set_for_ones_own_tenant_only() {
    let (app, key, key_id) = governable().await;

    let (status, view) = signed(
        &app,
        &key,
        &key_id,
        "PUT",
        "/v1/tenants/acme/limits",
        r#"{"tools":["read_file"],"models":["cheap"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["tools"], serde_json::json!(["read_file"]));
    assert_eq!(view["models"], serde_json::json!(["cheap"]));

    // `scope: "own"` again: the same administrator, a different tenant's limits — refused.
    let (refused, error) = signed(
        &app,
        &key,
        &key_id,
        "PUT",
        "/v1/tenants/other/limits",
        r#"{"tools":[]}"#,
    )
    .await;
    assert_eq!(refused, StatusCode::FORBIDDEN, "{error}");
}

#[tokio::test]
async fn an_empty_limit_is_not_the_same_as_no_limit() {
    let (app, key, key_id) = governable().await;

    // `[]` means "nothing permitted"; `null` means "no limit". Encoding one as the other would make a
    // deployment unable to express either.
    let (_, empty) = signed(
        &app,
        &key,
        &key_id,
        "PUT",
        "/v1/tenants/acme/limits",
        r#"{"tools":[]}"#,
    )
    .await;
    assert_eq!(empty["tools"], serde_json::json!([]));

    let (_, cleared) = signed(
        &app,
        &key,
        &key_id,
        "PUT",
        "/v1/tenants/acme/limits",
        r#"{"tools":null}"#,
    )
    .await;
    assert!(
        cleared.get("tools").is_none() || cleared["tools"].is_null(),
        "{cleared}"
    );
}

#[tokio::test]
async fn an_api_key_is_sealed_and_never_stored_in_the_clear() {
    let (seal, _bytes) = nucleos_auth::SealKey::generate();
    let (app, key, key_id) = governable_with(Some(seal.clone())).await;

    let script = r#"{"name":"primary","url":"https://example.invalid/v1/responses","model":"m","key":"sk-a-real-key"}"#;
    let (status, view) = signed(&app, &key, &key_id, "POST", "/v1/models", script).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    // The response says a key is configured and never what it is.
    assert_eq!(view["has_sealed_key"], true);
    assert!(!view.to_string().contains("sk-a-real-key"), "{view}");

    // The store holds a sealed value, not the key.
    let stored = app
        .system()
        .unwrap()
        .get(&format!(
            "{}{}",
            nucleosd::configure::SEALED_KEY_PREFIX,
            "primary"
        ))
        .await
        .unwrap()
        .expect("a sealed row was written");
    assert!(!stored.contains("sk-a-real-key"), "{stored}");
    assert!(seal.open(&stored).unwrap() == "sk-a-real-key");
}

#[tokio::test]
async fn an_api_key_without_a_seal_key_is_refused_rather_than_stored_in_the_clear() {
    let (app, key, key_id) = governable().await;
    let (status, error) = signed(
        &app,
        &key,
        &key_id,
        "POST",
        "/v1/models",
        r#"{"name":"primary","url":"https://example.invalid/v1/responses","model":"m","key":"sk"}"#,
    )
    .await;

    // The alternative — storing it anyway — would make every backup of the store a credential dump.
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{error}");
    assert!(
        error["reason"].as_str().unwrap().contains("seal key"),
        "{error}"
    );
}

#[tokio::test]
async fn sessions_can_be_listed_and_filtered_by_kind() {
    let (app, _, _) = governable().await;

    // Three sessions: two agent, one process — all owned by the same caller.
    for id in ["a1", "a2"] {
        let (status, _) = request(
            &app,
            "POST",
            "/v1/sessions",
            "",
            &format!(r#"{{"session":"{id}"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let all = body_json(
        router(Arc::clone(&app))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/sessions?limit=10")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    let ids: Vec<&str> = all["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["session"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"a1") && ids.contains(&"a2"), "{ids:?}");

    let agents = body_json(
        router(Arc::clone(&app))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/sessions?kind=agent&limit=10")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(agents["sessions"].as_array().unwrap().len(), 2);
    assert!(
        agents["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["kind"] == "agent")
    );

    // The filter is a filter, not a decoration: a kind with no sessions is an empty page, not every
    // session.
    let processes = body_json(
        router(Arc::clone(&app))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/sessions?kind=process&limit=10")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(processes["sessions"], serde_json::json!([]));

    // A misspelled kind is a client bug, not an empty page.
    let bad = router(Arc::clone(&app))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/sessions?kind=aghast")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
}

/// The page is chosen by the store, not narrowed afterwards.
///
/// A listing that asks the store for two rows and then discards the ones belonging to somebody else
/// answers with one row *and* a cursor: the client is told "there is more" and "you have seen
/// everything" by the same response. The subject has to be part of the query.
#[tokio::test]
async fn a_page_is_not_shortened_after_the_store_chose_it() {
    use nucleos::adapter::AllowAll;
    use nucleos::contract::{Principal, Routing};

    let store: Arc<dyn nucleos::adapter::Store> =
        Arc::new(nucleos_sqlite::SqliteStore::in_memory().unwrap());
    let kernel = Arc::new(
        nucleos::Kernel::builder()
            .model(
                "primary",
                Arc::new(nucleos_testkit::FakeModel::text(["unused"])),
            )
            .store(Arc::clone(&store))
            .policy(Arc::new(AllowAll))
            .routing(Routing {
                chain: vec!["primary".to_owned()],
                retries: 0,
            })
            .build()
            .unwrap(),
    );
    // The daemon's own principal is the caller when no tokens are configured, so this is "alice".
    let alice = Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    };
    let bob = Principal {
        tenant: "acme".to_owned(),
        subject: "bob".to_owned(),
    };
    let app = Arc::new(
        App::new(
            Arc::clone(&kernel),
            alice.clone(),
            vec!["primary".to_owned()],
        )
        .with_store(Arc::clone(&store)),
    );

    let spec = || nucleos::kernel::SessionSpec {
        credit_tokens: Some(1_000),
        ..Default::default()
    };
    kernel
        .open(&alice, nucleos::contract::SessionId::new("mine"), spec())
        .await
        .unwrap();
    for id in ["theirs-1", "theirs-2", "theirs-3"] {
        kernel
            .open(&bob, nucleos::contract::SessionId::new(id), spec())
            .await
            .unwrap();
    }

    let body = body_json(
        router(Arc::clone(&app))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/sessions?limit=2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;

    let sessions = body["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "only the caller's session: {body}");
    assert_eq!(sessions[0]["session"], "mine");
    assert!(
        body["next_page_token"].is_null(),
        "there is nothing after it, so there is no cursor: {body}"
    );
}

#[tokio::test]
async fn a_listing_pages_by_cursor() {
    let (app, _, _) = governable().await;
    for id in ["a1", "a2", "a3"] {
        request(
            &app,
            "POST",
            "/v1/sessions",
            "",
            &format!(r#"{{"session":"{id}"}}"#),
        )
        .await;
    }

    let first = body_json(
        router(Arc::clone(&app))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/sessions?kind=agent&limit=2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(first["sessions"].as_array().unwrap().len(), 2);
    let cursor = first["next_page_token"]
        .as_str()
        .expect("a third session exists");

    let second = body_json(
        router(Arc::clone(&app))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!(
                        "/v1/sessions?kind=agent&limit=2&page_token={cursor}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    let ids: Vec<&str> = second["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["session"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["a3"], "the cursor resumes after the page");
}

#[tokio::test]
async fn cancelling_a_turn_that_is_not_running_is_not_an_error() {
    let (app, _, _) = governable().await;
    request(&app, "POST", "/v1/sessions", "", r#"{"session":"s1"}"#).await;

    // A caller racing the end of a turn is a normal race. `404` would tell it the session is gone, and
    // `500` would tell it something broke — neither happened.
    let response = router(Arc::clone(&app))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/sessions/s1/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["cancelled"], false);
}

#[tokio::test]
async fn cancelling_another_subjects_session_is_not_found() {
    let (app, _, _) = governable().await;
    let response = router(Arc::clone(&app))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/sessions/nope/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // The same answer as an id that never existed: a refusal that says "this exists but is not yours"
    // is a directory of other people's sessions.
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Retention removes a closed session and leaves an open one, and the default call only reports.
#[tokio::test]
async fn prune_reports_first_then_removes_only_closed_sessions() {
    let (app, key, key_id) = governable().await;
    for id in ["s1", "s2"] {
        let (status, opened) = request(
            &app,
            "POST",
            "/v1/sessions",
            "",
            &format!(r#"{{"session":"{id}","credit_tokens":1000000}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{opened}");
    }
    let (status, _) = request(&app, "POST", "/v1/sessions/s1/close", "", "{}").await;
    assert_eq!(status, StatusCode::OK);

    // No `dry_run` field: the default is to report, and to remove nothing.
    let (status, report) = signed(
        &app,
        &key,
        &key_id,
        "POST",
        "/v1/sessions/prune",
        r#"{"before_unix_millis":4102444800000}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["sessions"], 1, "{report}");
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["sample"], serde_json::json!(["s1"]));
    let (status, _) = request(&app, "GET", "/v1/sessions/s1", "", "").await;
    assert_eq!(status, StatusCode::OK, "a dry run deleted something");

    // For real: the closed session goes, the open one stays.
    let (status, report) = signed(
        &app,
        &key,
        &key_id,
        "POST",
        "/v1/sessions/prune",
        r#"{"before_unix_millis":4102444800000,"dry_run":false}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["sessions"], 1);
    assert_eq!(report["dry_run"], false);

    let (status, _) = request(&app, "GET", "/v1/sessions/s1", "", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, view) = request(&app, "GET", "/v1/sessions/s2", "", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["state"], "open");
}

/// Retention is part of the configuration document: it exports, it applies, and it is validated.
#[tokio::test]
async fn retention_round_trips_through_the_configuration_document() {
    let (app, key, key_id) = governable().await;
    let document = r#"{
        "document_version": 1,
        "policy": "{\"default\":\"allow\",\"rules\":[]}",
        "models": [], "sources": [], "tenants": {},
        "retention": {"closed_session_ttl_millis": 3600000, "sweep_interval_millis": 60000}
    }"#;
    let (status, applied) = signed(&app, &key, &key_id, "PUT", "/v1/config", document).await;
    assert_eq!(status, StatusCode::OK, "{applied}");

    let (status, config) = signed(&app, &key, &key_id, "GET", "/v1/config", "").await;
    assert_eq!(status, StatusCode::OK, "{config}");
    assert_eq!(config["retention"]["closed_session_ttl_millis"], 3_600_000);
    assert_eq!(config["retention"]["sweep_interval_millis"], 60_000);

    // An interval with no TTL is a sweeper with nothing to do: refused, not stored.
    let invalid = r#"{
        "document_version": 1,
        "policy": "{\"default\":\"allow\",\"rules\":[]}",
        "models": [], "sources": [], "tenants": {},
        "retention": {"sweep_interval_millis": 60000}
    }"#;
    let (status, error) = signed(&app, &key, &key_id, "PUT", "/v1/config", invalid).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
}

/// The sweeper does nothing until retention is configured, and then removes only the closed session.
#[tokio::test]
async fn the_sweeper_is_disabled_until_retention_is_configured() {
    let (app, key, key_id) = governable().await;
    for id in ["s1", "s2"] {
        let (status, _) = request(
            &app,
            "POST",
            "/v1/sessions",
            "",
            &format!(r#"{{"session":"{id}","credit_tokens":1000000}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, _) = request(&app, "POST", "/v1/sessions/s1/close", "", "{}").await;
    assert_eq!(status, StatusCode::OK);

    // Unconfigured: nothing happens, and the closed session is still there.
    assert!(
        nucleosd::retention::sweep_once(&app)
            .await
            .unwrap()
            .is_none()
    );
    let (status, _) = request(&app, "GET", "/v1/sessions/s1", "", "").await;
    assert_eq!(status, StatusCode::OK);

    let document = r#"{
        "document_version": 1,
        "policy": "{\"default\":\"allow\",\"rules\":[]}",
        "models": [], "sources": [], "tenants": {},
        "retention": {"closed_session_ttl_millis": 1, "sweep_interval_millis": 60000}
    }"#;
    let (status, applied) = signed(&app, &key, &key_id, "PUT", "/v1/config", document).await;
    assert_eq!(status, StatusCode::OK, "{applied}");

    let report = nucleosd::retention::sweep_once(&app)
        .await
        .unwrap()
        .expect("retention is configured now");
    assert_eq!(report.sessions, 1);
    let (status, _) = request(&app, "GET", "/v1/sessions/s1", "", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = request(&app, "GET", "/v1/sessions/s2", "", "").await;
    assert_eq!(status, StatusCode::OK, "the open session survives a sweep");
}

/// `PUT /v1/config` is gated per subject, so a narrow administrator cannot rewrite the deployment.
///
/// The whole document used to be accepted on a good signature alone, with no `check_config` call at
/// all — the single mutation endpoint that skipped the gate. An administrator granted one subject could
/// replace the policy that had granted it, which is privilege escalation rather than a missing check.
#[tokio::test]
async fn a_document_that_changes_the_policy_needs_permission_for_it() {
    // Administrators here may change models and nothing else.
    const NARROW: &str = r#"{
        "default": "deny",
        "rules": [
          {"decision":"allow","config":"models","scope":"system","tenant":"acme","subject":"admin"}
        ]
    }"#;
    let (app, key, key_id) = governable_by(NARROW, None).await;

    // Changing the policy is refused: that subject is not theirs.
    let escalate = r#"{
        "document_version": 1,
        "policy": "{\"default\":\"allow\",\"rules\":[]}",
        "models": [], "sources": [], "tenants": {}
    }"#;
    let (status, refused) = signed(&app, &key, &key_id, "PUT", "/v1/config", escalate).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert!(
        refused["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("policy"),
        "the refusal names the subject: {refused}"
    );

    // Re-asserting the policy it already has changes nothing, so it needs no permission for it —
    // which is what keeps export → apply a no-op even on a deployment that locked itself down.
    let unchanged = serde_json::json!({
        "document_version": 1,
        "policy": NARROW,
        "models": [], "sources": [], "tenants": {},
    })
    .to_string();
    let (status, applied) = signed(&app, &key, &key_id, "PUT", "/v1/config", &unchanged).await;
    assert_eq!(status, StatusCode::OK, "{applied}");
}

/// A deployment that says nothing about isolation cannot open sessions.
///
/// The sandbox is decided at `open` and carried in the checkpoint, so it is gated there — once, for the
/// whole session. Before this, the policy field travelled from the request straight into the session, and
/// any tenant token could name `danger_full_access`.
#[tokio::test]
async fn a_policy_that_says_nothing_about_isolation_opens_no_sessions() {
    // Everything except isolation: models, tools, configuration — and no `sandbox` rule.
    const NO_ISOLATION: &str = r#"{
        "default": "deny",
        "rules": [
          {"decision":"allow","config":"*","scope":"system","tenant":"acme","subject":"admin"},
          {"decision":"allow","config":"*","scope":"own","tenant":"acme","subject":"admin"}
        ]
    }"#;
    let (app, _key, _key_id) = governable_by(NO_ISOLATION, None).await;

    let (status, refused) = request(&app, "POST", "/v1/sessions", "", r#"{"session":"s1"}"#).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert!(
        refused["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("workspace_write"),
        "the refusal names the isolation that was asked for: {refused}"
    );

    // Nothing was created: a refusal at the gate leaves no session behind.
    let (status, _) = request(&app, "GET", "/v1/sessions/s1", "", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// An HTTP source can be added by request, not only through a document.
///
/// The endpoint used to require `command` even when `url` was present, so the one transport added this
/// cycle could not be added through its own route. A url-only source now gets as far as the connection
/// attempt — which fails, here, because the host does not exist — instead of being refused as malformed.
#[tokio::test]
async fn an_http_source_is_added_by_url_rather_than_refused_for_a_missing_command() {
    let (app, key, key_id) = governable().await;
    let (status, error) = signed(
        &app,
        &key,
        &key_id,
        "POST",
        "/v1/sources",
        r#"{"name":"remote","url":"https://example.invalid/mcp"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "it reached the connection, not a validation error: {error}"
    );
    assert!(
        !error["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("needs a name and a command"),
        "a url-only source is not malformed: {error}"
    );
}

/// An administrator cannot enrol a key or issue a token for another tenant.
///
/// Both are identity minting, not configuration: the gate asks whether this caller may manage keys,
/// not whose identities they may create. Without the boundary, an `acme` administrator could enrol a
/// key for `default/admin` and inherit the bootstrap policy's system scope.
#[tokio::test]
async fn identity_minting_stays_inside_the_callers_tenant() {
    let (app, key, key_id) = governable().await;
    let other = nucleos_auth::KeyPair::generate().public("other", "bob");

    let (status, refused) = signed(
        &app,
        &key,
        &key_id,
        "POST",
        "/v1/admin/keys",
        &serde_json::json!({"key": other.key, "tenant": "other", "subject": "bob"}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");

    // The same key, for the caller's own tenant, is fine.
    let (status, enrolled) = signed(
        &app,
        &key,
        &key_id,
        "POST",
        "/v1/admin/keys",
        &serde_json::json!({"key": other.key, "tenant": "acme", "subject": "bob"}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{enrolled}");

    // Re-enrolling the same public key is a duplicate id, not a second identity.
    let (status, duplicate) = signed(
        &app,
        &key,
        &key_id,
        "POST",
        "/v1/admin/keys",
        &serde_json::json!({"key": other.key, "tenant": "acme", "subject": "alice"}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{duplicate}");

    // Tokens follow the same boundary.
    let (status, refused) = signed(
        &app,
        &key,
        &key_id,
        "POST",
        "/v1/tokens",
        r#"{"tenant":"other","subject":"bob"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");

    let (status, issued) = signed(
        &app,
        &key,
        &key_id,
        "POST",
        "/v1/tokens",
        r#"{"tenant":"acme","subject":"alice"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{issued}");
}

/// An HTTP source must use TLS, and a token for it is sealed and never returned.
#[tokio::test]
async fn an_http_source_uses_tls_and_seals_its_token() {
    let (app, key, key_id) = governable().await;
    let (status, error) = signed(
        &app,
        &key,
        &key_id,
        "POST",
        "/v1/sources",
        r#"{"name":"remote","url":"http://example.invalid/mcp"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    assert!(
        error["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("https"),
        "{error}"
    );

    // With a seal key, a token is stored sealed — before the connection is attempted.
    let (app, key, key_id) = governable_with(Some(nucleos_auth::SealKey::generate().0)).await;
    let (status, _) = signed(
        &app,
        &key,
        &key_id,
        "POST",
        "/v1/sources",
        r#"{"name":"remote","url":"https://example.invalid/mcp","auth_token":"a-real-secret"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let sealed = app
        .system()
        .unwrap()
        .get("config/keys/source/remote")
        .await
        .unwrap()
        .expect("the token was stored");
    assert!(
        !sealed.contains("a-real-secret"),
        "stored in the clear: {sealed}"
    );

    // Without one, a token cannot be stored at all.
    let (app, key, key_id) = governable().await;
    let (status, error) = signed(
        &app,
        &key,
        &key_id,
        "POST",
        "/v1/sources",
        r#"{"name":"remote","url":"https://example.invalid/mcp","auth_token":"a-real-secret"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        error["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("no seal key"),
        "{error}"
    );
}
