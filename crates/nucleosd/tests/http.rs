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

    #[cfg(target_os = "macos")]
    {
        // Seatbelt is available, so the process starts confined instead of being refused.
        assert_eq!(response.status(), StatusCode::OK);
    }
    #[cfg(not(target_os = "macos"))]
    {
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
async fn a_reload_publishes_a_new_generation_and_names_what_changed() {
    use nucleos_mcp::McpSource;

    // One fixture, shared with the MCP crate's own tests rather than copied: two copies of a fake
    // server is two things that drift.
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../nucleos-mcp/tests/fixtures/echo_server.py"
    );
    let source = McpSource::new("echo", "python3").with_args(["-u", script]);
    let tools: Vec<Arc<dyn nucleos::adapter::Tool>> = source
        .connect()
        .await
        .unwrap()
        .into_iter()
        .map(|tool| Arc::new(tool) as Arc<dyn nucleos::adapter::Tool>)
        .collect();
    let names: Vec<String> = tools.iter().map(|tool| tool.spec().name).collect();

    let mut builder = nucleos::Kernel::builder()
        .model("primary", Arc::new(nucleos_testkit::FakeModel::text(["unused"])))
        .store(Arc::new(nucleos_sqlite::SqliteStore::in_memory().unwrap()))
        .policy(Arc::new(nucleos::adapter::AllowAll))
        .clock(Arc::new(nucleos_testkit::FixedClock::at(0)))
        .routing(nucleos::contract::Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        });
    for tool in tools {
        builder = builder.tool(tool);
    }
    let kernel = Arc::new(builder.build().unwrap());
    let before = kernel.generation();

    let app = Arc::new(
        App::new(
            Arc::clone(&kernel),
            nucleos::contract::Principal {
                tenant: "acme".to_owned(),
                subject: "alice".to_owned(),
            },
            vec!["primary".to_owned()],
        )
        .with_tools(names.clone())
        .with_source(source),
    );

    let response = router(Arc::clone(&app))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/tools/reload")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["tools"], serde_json::json!(["echo"]));
    assert_eq!(body["added"], serde_json::json!([]), "the same server");
    assert!(body["generation"].as_u64().unwrap() > before.0);

    // `Describe` reports the generation new sessions are pinned to, so a caller can tell which set
    // it is talking to.
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
    assert_eq!(described["generation"], body["generation"]);
}

#[tokio::test]
async fn a_reload_without_a_source_says_so() {
    let response = router(running())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/tools/reload")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Not a silent no-op: a reload that did nothing is a caller believing it deployed something.
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let error = body_json(response).await;
    assert!(error["reason"].as_str().unwrap().contains("NUCLEOS_MCP"));
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

/// A daemon that requires a bearer token.
fn guarded() -> Arc<App> {
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
    Arc::new(
        App::new(
            Arc::new(kernel),
            nucleos::contract::Principal {
                tenant: "acme".to_owned(),
                subject: "alice".to_owned(),
            },
            vec!["primary".to_owned()],
        )
        .with_token(Some("s3cret".to_owned())),
    )
}

#[tokio::test]
async fn a_configured_token_is_required_on_every_route() {
    let app = guarded();

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
                .header("authorization", "Bearer s3cret")
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
                .header("authorization", "Bearer wrong")
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
    let app = guarded();

    // One allowed request, one refused, one reload attempt with no source.
    let _ = router(Arc::clone(&app))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/describe")
                .header("authorization", "Bearer s3cret")
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

    let body = body_json(
        router(Arc::clone(&app))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/metrics")
                    .header("authorization", "Bearer s3cret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;

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

fn multi_tenant() -> Arc<App> {
    use nucleos::adapter::AllowAll;

    let directory = std::env::temp_dir().join(format!("nucleos-tenants-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let file = directory.join("tokens.json");
    std::fs::write(
        &file,
        r#"{"tokens":[
            {"token":"acme-token","tenant":"acme","subject":"alice"},
            {"token":"other-token","tenant":"other","subject":"bob"}
        ]}"#,
    )
    .unwrap();

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

    Arc::new(
        App::new(
            Arc::new(kernel),
            nucleos::contract::Principal::default(),
            Vec::new(),
        )
        .with_tokens(nucleosd::auth::Tokens::load(&file).unwrap()),
    )
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
    let app = multi_tenant();

    let (created, _) = request(
        &app,
        "POST",
        "/v1/sessions",
        "acme-token",
        r#"{"session":"s1"}"#,
    )
    .await;
    assert_eq!(created, StatusCode::OK);

    // The owner can read it.
    let (mine, _) = request(&app, "GET", "/v1/sessions/s1", "acme-token", "").await;
    assert_eq!(mine, StatusCode::OK);

    // Another tenant gets the same answer as for an id that never existed. A 403 would tell them
    // which identifiers their neighbours are using.
    let (theirs, error) = request(&app, "GET", "/v1/sessions/s1", "other-token", "").await;
    assert_eq!(theirs, StatusCode::NOT_FOUND);
    assert_eq!(error["code"], "not_found");
}

#[tokio::test]
async fn the_same_identifier_in_two_tenants_is_two_sessions() {
    let app = multi_tenant();

    // Callers choose their ids, so "s1" from two tenants is ordinary. The store's key is
    // (tenant, id), so both succeed and neither sees the other's history.
    let (first, _) = request(&app, "POST", "/v1/sessions", "acme-token", r#"{"session":"s1"}"#).await;
    let (second, _) = request(&app, "POST", "/v1/sessions", "other-token", r#"{"session":"s1"}"#).await;
    assert_eq!(first, StatusCode::OK);
    assert_eq!(second, StatusCode::OK, "distinct keys, not a collision");

    // Each tenant reads its own stream. The events route answers SSE, so this asks for the status
    // rather than trying to parse a body.
    for token in ["acme-token", "other-token"] {
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
    let app = multi_tenant();

    request(&app, "POST", "/v1/sessions", "acme-token", r#"{"session":"a1"}"#).await;
    request(&app, "POST", "/v1/sessions", "other-token", r#"{"session":"b1"}"#).await;

    // Different ids, so no collision: each caller sees only its own.
    assert_eq!(
        request(&app, "GET", "/v1/sessions/a1", "acme-token", "").await.0,
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "GET", "/v1/sessions/b1", "other-token", "").await.0,
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "GET", "/v1/sessions/b1", "acme-token", "").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_token_that_is_not_in_the_document_is_refused() {
    let app = multi_tenant();
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
        .model("primary", Arc::new(nucleos_testkit::FakeModel::text(["unused"])))
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

    let key = nucleos::adapter::SessionKey::new(
        "acme",
        nucleos::contract::SessionId::new("s1"),
    );
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
    assert_eq!(view["partial"]["text"], "check the config, then run migrate");
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
        .set(nucleosd::http::POLICY_KEY, r#"{"default":"deny","rules":[]}"#)
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
