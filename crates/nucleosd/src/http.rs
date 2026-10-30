//! The HTTP surface: the kernel's syscalls, and one convenience endpoint that drives them.
//!
//! Nothing here decides anything. Each handler loads the session from its checkpoint, calls one
//! syscall, and serializes what came back — which is why no handler contains a branch that
//! changes behaviour.
//!
//! Loading per request is not a shortcut: every syscall may be served by a different process, so
//! the checkpoint is the session, and a handler that kept one in memory would be the only thing
//! that could disagree with it.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, get, post};
use axum::{Json, Router};
use nucleos::contract::{ErrorCode, Principal, SessionId, SessionState};
use nucleos_sqlite::SqliteStore;
use nucleos::kernel::{HitlVerdict, Kernel, SessionSpec, StateWrite};
use nucleos::wire::api::{
    DescribeResponse, ErrorBody, InferenceRequest, InferenceResponse, ProcessInfo,
    MetricsResponse, ProcessListResponse, ProcessStartRequest, ProcessStartResponse, ReloadResponse,
    SessionView, StartRequest,
    StartResponse, StateRequest, StdinRequest, ToolBatchRequest, ToolBatchResponse, VerdictRequest,
};
use nucleos::wire::openapi;
use nucleos_agent as agent;
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::StreamExt;

/// The ABI version this daemon speaks.
pub const ABI_VERSION: &str = "v1";

/// The authenticated caller, as the middleware left it on the request.
///
/// An extractor rather than a field on `App`: with more than one token the daemon's principal is not
/// the caller's, and reading it from the app would attribute every session to whoever the deployment
/// configured first.
pub struct Caller(pub Principal);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Principal>()
            .cloned()
            .map(Caller)
            .ok_or_else(|| {
                // Only reachable if a route were mounted outside the authentication layer, which is
                // a wiring bug rather than a caller error.
                ApiError::from_error(&nucleos::Error::internal(
                    "no authenticated principal on the request",
                ))
            })
    }
}

/// What every handler shares.
pub struct App {
    /// `None` until a deployment is configured.
    kernel: Option<Arc<Kernel>>,
    /// The principal every call is attributed to.
    ///
    /// Single-tenant for now. The unix socket's peer credentials are the authentication this
    /// daemon can honestly claim; deriving a principal from them is the next slice.
    principal: Principal,
    models: Vec<String>,
    /// The tools every source advertises, for `Describe`.
    ///
    /// Behind a lock because a reload replaces it: it describes the *current* generation, and a
    /// snapshot taken at start-up would go on describing a set nobody is pinned to any more.
    tools: std::sync::RwLock<Vec<String>>,
    /// The source a reload reconnects, when the deployment configured one.
    source: Option<nucleos_mcp::McpSource>,
    /// What this daemon has done since it started.
    metrics: crate::metrics::Metrics,
    /// Who may call, and who each caller is.
    ///
    /// With no tokens configured the socket's permissions are the authentication and every call is
    /// attributed to one principal — a decision about where the daemon is listening, not one this
    /// struct can make for it.
    tokens: crate::auth::Tokens,
}

impl App {
    /// Serve a kernel.
    #[must_use]
    pub fn new(kernel: Arc<Kernel>, principal: Principal, models: Vec<String>) -> Self {
        Self {
            kernel: Some(kernel),
            tokens: crate::auth::Tokens::confined_to(principal.clone()),
            principal,
            models,
            tools: std::sync::RwLock::new(Vec::new()),
            source: None,
            metrics: crate::metrics::Metrics::started_now(),
        }
    }

    /// The counters, for a handler and for a test.
    #[must_use]
    pub fn metrics(&self) -> &crate::metrics::Metrics {
        &self.metrics
    }

    /// Require one of these tokens on every request.
    #[must_use]
    pub fn with_tokens(mut self, tokens: crate::auth::Tokens) -> Self {
        self.tokens = tokens;
        self
    }

    /// Require one token on every request, attributed to the principal this daemon already had.
    #[must_use]
    pub fn with_token(self, token: Option<String>) -> Self {
        match token.filter(|token| !token.is_empty()) {
            Some(token) => {
                let principal = self.principal.clone();
                let tokens = crate::auth::Tokens::single(token, principal);
                self.with_tokens(tokens)
            }
            None => self,
        }
    }

    /// Remember the tool source, so `POST /v1/tools/reload` has something to reconnect.
    #[must_use]
    pub fn with_source(mut self, source: nucleos_mcp::McpSource) -> Self {
        self.source = Some(source);
        self
    }

    /// Advertise the tools that were discovered, so a caller can see what it may ask for.
    #[must_use]
    pub fn with_tools(self, tools: Vec<String>) -> Self {
        if let Ok(mut current) = self.tools.write() {
            *current = tools;
        }
        self
    }

    /// The tools in force.
    fn tools_now(&self) -> Vec<String> {
        self.tools
            .read()
            .map(|tools| tools.clone())
            .unwrap_or_default()
    }

    /// Serve what the environment describes.
    ///
    /// Three variables, because a deployment file is not written yet and a daemon that can only be
    /// tested with fakes is a daemon nobody runs:
    ///
    /// ```text
    /// NUCLEOS_MODEL_URL    full chat-completions URL
    /// NUCLEOS_MODEL_NAME   the model to ask for
    /// NUCLEOS_MODEL_KEY    bearer token, when the endpoint wants one
    /// NUCLEOS_CONTEXT      context window, default 128000
    /// NUCLEOS_MCP          one tool source: `name=command args…`
    /// ```
    ///
    /// Unset means unconfigured: up, describing itself, refusing to pretend it has models.
    pub async fn from_env(layout: &crate::layout::Layout) -> Result<Self, nucleos::Error> {
        let Ok(url) = std::env::var("NUCLEOS_MODEL_URL") else {
            return Ok(Self::unconfigured());
        };
        let model = std::env::var("NUCLEOS_MODEL_NAME")
            .map_err(|_| nucleos::Error::invalid("NUCLEOS_MODEL_NAME is required with a url"))?;
        let context_window = std::env::var("NUCLEOS_CONTEXT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(128_000);

        let adapter = nucleos_openai::OpenAi::new(nucleos_openai::Config {
            url,
            api_key: std::env::var("NUCLEOS_MODEL_KEY").ok(),
            model: model.clone(),
            capability: nucleos_openai::Config::capabilities(context_window, 8_192),
            options: nucleos::contract::Hints {
                // Refused rather than silently applied: dropping the oldest items mid-session
                // would make the provider's view of the conversation differ from the kernel's,
                // and compaction is the kernel's job, not the transport's.
                truncation: Some(nucleos::contract::Truncation::Disabled),
                ..Default::default()
            },
            // Derived from the session, not chosen by the application: a cache key that changes per
            // call bills the same prefix twice.
            prompt_cache_key: None,
            include: Vec::new(),
            safety_identifier: None,
            // Both are statements about the deployment, so both are variables rather than
            // guesses: strict mode rejects a schema that is not strict-eligible, and reasoning
            // effort is rejected by models that do not have it.
            strict_tools: std::env::var("NUCLEOS_STRICT_TOOLS").is_ok(),
            supports_reasoning: std::env::var("NUCLEOS_REASONING").is_ok(),
            timeout: Some(std::time::Duration::from_secs(120)),
        })?;

        // One source, from one variable: a deployment file is the next slice, and a daemon that
        // cannot run a real tool is a daemon nobody can test against.
        let mut tools: Vec<Arc<dyn nucleos::adapter::Tool>> = Vec::new();
        let mut discovered: Vec<String> = Vec::new();
        let mut configured_source: Option<nucleos_mcp::McpSource> = None;
        if let Ok(spec) = std::env::var("NUCLEOS_MCP") {
            let (name, command) = spec.split_once('=').ok_or_else(|| {
                nucleos::Error::invalid("NUCLEOS_MCP must be `name=command args…`")
            })?;
            let mut parts = command.split_whitespace();
            let command = parts
                .next()
                .ok_or_else(|| nucleos::Error::invalid("NUCLEOS_MCP names no command"))?;
            let source = nucleos_mcp::McpSource::new(name.trim(), command).with_args(parts);
            use nucleos::adapter::Tool as _;
            for tool in source.connect().await? {
                discovered.push(tool.spec().name);
                tools.push(Arc::new(tool));
            }
            configured_source = Some(source);
        }

        let routing = nucleos::contract::Routing {
            chain: vec![model.clone()],
            retries: 1,
        };
        // Two files with two lifecycles: configuration is versioned and kept, sessions are pruned.
        // Both are named inside one root, so a second instance on this host is one more `--root`.
        layout
            .create()
            .map_err(|error| {
                nucleos::Error::unavailable(format!(
                    "cannot create {}: {error}",
                    layout.root.display()
                ))
            })?;
        let system = nucleos_sqlite::SystemStore::open(layout.system_store())?;
        tracing::debug!(
            event = "system_store_open",
            path = %layout.system_store().display(),
            schema = system.schema_version()?,
        );
        let store = SqliteStore::open(layout.session_store())?;
        // Two shapes, one meaning: a token file for a deployment with several callers, a single
        // token for one, and neither means the socket's permissions are the authentication.
        let tokens = match std::env::var("NUCLEOS_TOKENS") {
            Ok(path) => crate::auth::Tokens::load(&path)?,
            Err(_) => match std::env::var("NUCLEOS_TOKEN") {
                Ok(token) if !token.is_empty() => crate::auth::Tokens::single(
                    token,
                    nucleos::contract::Principal {
                        tenant: std::env::var("NUCLEOS_TENANT")
                            .unwrap_or_else(|_| "default".to_owned()),
                        subject: std::env::var("NUCLEOS_SUBJECT")
                            .unwrap_or_else(|_| "local".to_owned()),
                    },
                ),
                Ok(_) => {
                    return Err(nucleos::Error::invalid(
                        "NUCLEOS_TOKEN is set but empty: unset it to rely on the socket, or give \
                         it a value",
                    ));
                }
                Err(_) => crate::auth::Tokens::confined_to(Self::unconfigured().principal),
            },
        };
        let policy = load_policy(std::env::var("NUCLEOS_POLICY").ok())?;
        let mut builder = Kernel::builder()
            .model(&model, Arc::new(adapter))
            .store(Arc::new(store))
            .policy(policy)
            .routing(routing);
        for tool in tools {
            builder = builder.tool(tool);
        }
        let kernel = builder.build()?;

        let app = Self::new(
            Arc::new(kernel),
            Self::unconfigured().principal,
            vec![model],
        )
        .with_tools(discovered)
        .with_tokens(tokens);
        Ok(match configured_source {
            Some(source) => app.with_source(source),
            None => app,
        })
    }

    /// Serve nothing but `describe`: the daemon is up, no deployment is configured.
    #[must_use]
    pub fn unconfigured() -> Self {
        Self {
            kernel: None,
            principal: Principal {
                tenant: "default".to_owned(),
                subject: "local".to_owned(),
            },
            models: Vec::new(),
            tools: std::sync::RwLock::new(Vec::new()),
            source: None,
            tokens: crate::auth::Tokens::default(),
            metrics: crate::metrics::Metrics::started_now(),
        }
    }

    fn kernel(&self) -> Result<&Arc<Kernel>, ApiError> {
        self.kernel.as_ref().ok_or_else(|| {
            ApiError::from_error(&nucleos::Error::unavailable(
                "no deployment is configured: the daemon has no models or tools",
            ))
        })
    }
}

/// One route: what it is, and how it is served.
///
/// The two halves are one row on purpose. A table for the document and a chain of `.route()`
/// calls for the server drift, and nothing notices: a `Router` does not expose its routes, so no
/// test can compare them. Here the verb sits three lines above the handler that serves it, and
/// `tests/http.rs` proves the pair by sending a request.
pub struct Route {
    /// What the published document says about it.
    pub meta: openapi::Route,
    /// How it is served.
    pub build: fn() -> MethodRouter<Arc<App>>,
}

/// Every route the daemon serves.
pub const ROUTES: &[Route] = &[
    Route {
        meta: openapi::Route {
            method: "get",
            path: "/v1/describe",
            operation: "describe",
            summary: "Capabilities, models, and the ABI version.",
            request: None,
            reply: openapi::Reply::Json("DescribeResponse"),
        },
        build: || get(describe),
    },
    Route {
        meta: openapi::Route {
            method: "post",
            path: "/v1/agents",
            operation: "runAgent",
            summary: "Open a session and run the reference loop to completion.",
            request: Some("StartRequest"),
            reply: openapi::Reply::Json("StartResponse"),
        },
        build: || post(run_agent),
    },
    Route {
        meta: openapi::Route {
            method: "post",
            path: "/v1/sessions",
            operation: "openSession",
            summary: "Open a session. Drives nothing: the caller decides each turn.",
            request: Some("StartRequest"),
            reply: openapi::Reply::Json("StartResponse"),
        },
        build: || post(open),
    },
    Route {
        meta: openapi::Route {
            method: "get",
            path: "/v1/sessions/{id}",
            operation: "viewSession",
            summary: "Where a session is, what it has spent, and what it waits on.",
            request: None,
            reply: openapi::Reply::Json("SessionView"),
        },
        build: || get(view),
    },
    Route {
        meta: openapi::Route {
            method: "post",
            path: "/v1/sessions/{id}/inference",
            operation: "inference",
            summary: "One model call. Appends nothing: what you send is what you get.",
            request: Some("InferenceRequest"),
            reply: openapi::Reply::Json("InferenceResponse"),
        },
        build: || post(inference),
    },
    Route {
        meta: openapi::Route {
            method: "post",
            path: "/v1/sessions/{id}/tools",
            operation: "invokeTools",
            summary: "Run a batch of tool calls. Policy is asked about all of them first.",
            request: Some("ToolBatchRequest"),
            reply: openapi::Reply::Json("ToolBatchResponse"),
        },
        build: || post(tools),
    },
    Route {
        meta: openapi::Route {
            method: "post",
            path: "/v1/sessions/{id}/state",
            operation: "appendState",
            summary: "Append messages, or write the application's own opaque state.",
            request: Some("StateRequest"),
            reply: openapi::Reply::NoContent,
        },
        build: || post(append),
    },
    Route {
        meta: openapi::Route {
            method: "post",
            path: "/v1/processes",
            operation: "startProcess",
            summary: "Start a command as a process session, under a sandbox policy.",
            request: Some("ProcessStartRequest"),
            reply: openapi::Reply::Json("ProcessStartResponse"),
        },
        build: || post(start_process),
    },
    Route {
        meta: openapi::Route {
            method: "get",
            path: "/v1/processes",
            operation: "listProcesses",
            summary: "The processes this node is running. Not durable: memory, not the log.",
            request: None,
            reply: openapi::Reply::Json("ProcessListResponse"),
        },
        build: || get(list_processes),
    },
    Route {
        meta: openapi::Route {
            method: "post",
            path: "/v1/sessions/{id}/stdin",
            operation: "writeStdin",
            summary: "Write to a running process's stdin. Recorded before it is sent.",
            request: Some("StdinRequest"),
            reply: openapi::Reply::NoContent,
        },
        build: || post(write_stdin),
    },
    Route {
        meta: openapi::Route {
            method: "post",
            path: "/v1/sessions/{id}/signal",
            operation: "signalProcess",
            summary: "Ask a running process to end.",
            request: None,
            reply: openapi::Reply::NoContent,
        },
        build: || post(signal_process),
    },
    Route {
        meta: openapi::Route {
            method: "post",
            path: "/v1/sessions/{id}/wait",
            operation: "waitProcess",
            summary: "Wait for a process to end, and say how it ended.",
            request: None,
            reply: openapi::Reply::Json("ExecOutput"),
        },
        build: || post(wait_process),
    },
    Route {
        meta: openapi::Route {
            method: "get",
            path: "/v1/metrics",
            operation: "metrics",
            summary: "What this daemon has counted since it started.",
            request: None,
            reply: openapi::Reply::Json("MetricsResponse"),
        },
        build: || get(metrics),
    },
    Route {
        meta: openapi::Route {
            method: "post",
            path: "/v1/tools/reload",
            operation: "reloadTools",
            summary: "Reconnect the tool source and publish a new generation.",
            request: None,
            reply: openapi::Reply::Json("ReloadResponse"),
        },
        build: || post(reload),
    },
    Route {
        meta: openapi::Route {
            method: "post",
            path: "/v1/sessions/{id}/configure",
            operation: "configureSession",
            summary: "Change what a session is doing, without closing it.",
            request: Some("ConfigPatch"),
            reply: openapi::Reply::Json("ConfigureResponse"),
        },
        build: || post(configure),
    },
    Route {
        meta: openapi::Route {
            method: "post",
            path: "/v1/sessions/{id}/close",
            operation: "closeSession",
            summary: "Close a session. Further calls are refused.",
            request: None,
            reply: openapi::Reply::Json("StartResponse"),
        },
        build: || post(close),
    },
    Route {
        meta: openapi::Route {
            method: "post",
            path: "/v1/sessions/{id}/verdict",
            operation: "verdict",
            summary: "Answer a call parked on a human, then run the reference loop on.",
            request: Some("VerdictRequest"),
            reply: openapi::Reply::Json("StartResponse"),
        },
        build: || post(verdict),
    },
    Route {
        meta: openapi::Route {
            method: "get",
            path: "/v1/sessions/{id}/events",
            operation: "events",
            summary: "The event stream. `id:` is the event's `seq`, so Last-Event-ID resumes.",
            request: None,
            reply: openapi::Reply::Stream,
        },
        build: || get(events),
    },
];

/// The router, built from the same table the document is.
pub fn router(app: Arc<App>) -> Router {
    ROUTES
        .iter()
        .fold(Router::new(), |router, route| {
            router.route(route.meta.path, (route.build)())
        })
        // One layer over every route, including `describe`: a surface exempted from authentication
        // is a surface that tells an unauthenticated caller what the deployment has.
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&app),
            authenticate,
        ))
        // Outside the authentication layer: a request refused for its token is still a request, and
        // a counter that ignores refusals hides the one number an operator looks at first.
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&app),
            observe,
        ))
        .with_state(app)
}

/// Count one request, and write one line about it.
async fn observe(
    State(app): State<Arc<App>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let started = std::time::Instant::now();
    let method = request.method().to_string();
    // The template, not the path: `/v1/sessions/{id}` is one route, however many sessions exist.
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|path| path.as_str().to_owned())
        .unwrap_or_else(|| request.uri().path().to_owned());

    let response = next.run(request).await;
    let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    app.metrics.observe(&method, &route, response.status().as_u16());
    tracing::info!(
        kind = "request",
        method = %method,
        route = %route,
        status = response.status().as_u16(),
        duration_millis = millis,
    );
    response
}

/// Refuse a request that does not carry the token, when one is configured.
async fn authenticate(
    State(app): State<Arc<App>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    match app.tokens.authenticate(header) {
        Ok(principal) => {
            // The identity travels with the request: a handler that read it from the app would be
            // reading the daemon's, which is the same only when there is one caller.
            let mut request = request;
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Err(error) => {
            tracing::warn!(event = "request_refused", reason = %error.reason());
            let (status, body) = crate::auth::refusal(&error);
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::UNAUTHORIZED);
            (status, Json(body)).into_response()
        }
    }
}

/// The table, for the OpenAPI generator.
#[must_use]
pub fn openapi_routes() -> Vec<openapi::Route> {
    ROUTES.iter().map(|route| route.meta).collect()
}

async fn describe(State(app): State<Arc<App>>) -> Json<DescribeResponse> {
    Json(DescribeResponse {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        abi_version: ABI_VERSION.to_owned(),
        models: app.models.clone(),
        tools: app.tools_now(),
        generation: app.kernel.as_ref().map(|kernel| kernel.generation().0).unwrap_or(0),
    })
}

/// What this daemon has counted.
async fn metrics(State(app): State<Arc<App>>) -> Json<MetricsResponse> {
    Json(app.metrics().snapshot())
}

/// Reconnect the tool source and publish what it advertises now.
///
/// This is the whole point of generations from the outside: a source is a process somebody else
/// deploys, and reconnecting it must not be a restart. Sessions already running keep the set they
/// are pinned to; sessions that start from now on get this one.
async fn reload(State(app): State<Arc<App>>) -> Result<Json<ReloadResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let source = app.source.as_ref().ok_or_else(|| {
        ApiError::from_error(&nucleos::Error::unavailable(
            "no tool source is configured: `NUCLEOS_MCP` names one, and a reload reconnects it",
        ))
    })?;

    let before = app.tools_now();
    let tools: Vec<Arc<dyn nucleos::adapter::Tool>> = source
        .connect()
        .await
        .map_err(|error| ApiError::from_error(&error))?
        .into_iter()
        .map(|tool| Arc::new(tool) as Arc<dyn nucleos::adapter::Tool>)
        .collect();

    let names: Vec<String> = tools.iter().map(|tool| tool.spec().name).collect();
    let generation = kernel
        .publish(tools)
        .map_err(|error| ApiError::from_error(&error))?;
    app.metrics().reloaded();

    let added: Vec<String> = names
        .iter()
        .filter(|name| !before.contains(name))
        .cloned()
        .collect();
    let removed: Vec<String> = before
        .iter()
        .filter(|name| !names.contains(name))
        .cloned()
        .collect();
    tracing::info!(
        event = "tools_reloaded",
        generation = generation.0,
        tools = names.len(),
        added = ?added,
        removed = ?removed,
    );
    if let Ok(mut current) = app.tools.write() {
        *current = names.clone();
    }

    Ok(Json(ReloadResponse {
        generation: generation.0,
        tools: names,
        added,
        removed,
    }))
}

/// Open a session and run the reference loop to completion.
async fn run_agent(
    State(app): State<Arc<App>>,
    caller: Caller,
    Json(request): Json<StartRequest>,
) -> Result<Json<StartResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    app.metrics().session_opened();
    let spec: SessionSpec = request.clone().into();
    let id = SessionId::new(request.session_id());
    let outcome = if request.resume {
        agent::resume(&kernel, &caller.0, &id).await
    } else {
        agent::start(&kernel, &caller.0, id, spec).await
    }
    .map_err(|error| ApiError::from_error(&error))?;
    Ok(Json(StartResponse::from(&outcome)))
}

/// Open a session without driving it.
async fn open(
    State(app): State<Arc<App>>,
    caller: Caller,
    Json(request): Json<StartRequest>,
) -> Result<Json<StartResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    app.metrics().session_opened();
    let spec: SessionSpec = request.clone().into();
    let session = kernel
        .open(&caller.0, SessionId::new(request.session_id()), spec)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    Ok(Json(StartResponse {
        session: session.id().as_str().to_owned(),
        state: SessionState::Open,
        stop: None,
        next_seq: session.next_seq(),
    }))
}

async fn view(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<SessionView>, ApiError> {
    let kernel = app.kernel()?.clone();
    let session = load(&kernel, &caller, &id).await?;
    Ok(Json(SessionView {
        session: session.id().as_str().to_owned(),
        state: if session.is_closed() {
            SessionState::Closed
        } else {
            SessionState::Open
        },
        next_seq: session.next_seq(),
        inference_calls: session.inference_calls(),
        pending: session.pending().to_vec(),
        usage: session.usage().clone(),
        process: session.process().cloned(),
        sandbox: session.sandbox().clone(),
        workspace: session.workspace().cloned(),
        partial: kernel
            .partial(&session)
            .await
            .map_err(|error| ApiError::from_error(&error))?,
    }))
}

async fn inference(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Json(request): Json<InferenceRequest>,
) -> Result<Json<InferenceResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let mut session = load(&kernel, &caller, &id).await?;
    let outcome = kernel
        .inference(&mut session, &request.messages)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    Ok(Json(InferenceResponse {
        message: outcome.message,
        finish: format!("{:?}", outcome.finish).to_lowercase(),
        usage: outcome.usage,
    }))
}

async fn tools(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Json(request): Json<ToolBatchRequest>,
) -> Result<Json<ToolBatchResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let mut session = load(&kernel, &caller, &id).await?;
    let outputs = kernel
        .invoke_tools(&mut session, &request.calls)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    Ok(Json(ToolBatchResponse {
        results: outputs
            .into_iter()
            .zip(&request.calls)
            .map(|(output, call)| nucleos::contract::ToolResult {
                call_id: call.id.clone(),
                content: output.content,
                is_error: output.is_error,
            })
            .collect(),
    }))
}

async fn append(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Json(request): Json<StateRequest>,
) -> Result<StatusCode, ApiError> {
    let kernel = app.kernel()?.clone();
    let mut session = load(&kernel, &caller, &id).await?;
    kernel
        .append(
            &mut session,
            StateWrite {
                messages: request.messages,
                data: request.data,
            },
        )
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Start a command as a process session.
///
/// The session is created by this call, so the sandbox policy travels with it: a process is the one
/// thing whose *launch* has to be gated, and the gate needs to see the policy and the argv together.
async fn start_process(
    State(app): State<Arc<App>>,
    caller: Caller,
    Json(request): Json<ProcessStartRequest>,
) -> Result<Json<ProcessStartResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    app.metrics().session_opened();
    let id = SessionId::new(format!(
        "proc-{:016x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    ));

    let spec = SessionSpec {
        kind: nucleos::contract::SessionKind::Process,
        process: Some(nucleos::contract::ProcessSpec {
            argv: request.argv,
            cwd: request.cwd,
            env: request.env,
            permissions: request.permissions,
        }),
        sandbox: request.sandbox.policy,
        workspace: request.sandbox.workspace,
        budget: nucleos::contract::Budget {
            max_wall_millis: request.timeout_millis,
            ..Default::default()
        },
        credit_tokens: 0,
        ..SessionSpec::default()
    };

    let session = kernel
        .spawn(&caller.0, id, spec)
        .await
        .map_err(|error| ApiError::from_error(&error))?;

    let process = session.process().cloned().ok_or_else(|| {
        ApiError::from_error(&nucleos::Error::internal(
            "a process session without a process",
        ))
    })?;
    Ok(Json(ProcessStartResponse {
        session: session.id().as_str().to_owned(),
        pid: match process.status {
            nucleos::contract::ProcessStatus::Running { pid } => Some(pid),
            _ => None,
        },
        state: process.status,
        next_seq: session.next_seq(),
    }))
}

/// What is running on this node right now.
///
/// The supervisor's memory, not the store: a session that outlived a restart is absent here and
/// reports `Lost` in its own view. A durable list needs the store to enumerate sessions, which it
/// cannot do yet.
/// Query parameters for the process listing.
#[derive(Debug, serde::Deserialize)]
struct ProcessesQuery {
    /// `pending`, `running`, `exited` or `lost`.
    #[serde(default)]
    phase: Option<String>,
    /// Page size. Zero asks for the default.
    #[serde(default)]
    limit: u32,
    /// Continue after this session id.
    #[serde(default)]
    page_token: Option<String>,
}

async fn list_processes(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(query): Query<ProcessesQuery>,
) -> Result<Json<ProcessListResponse>, ApiError> {
    let kernel = app.kernel()?.clone();

    // A phase this build does not know is a client bug, not an empty page: answering "nothing" to
    // a misspelled filter is how a caller concludes the feature is broken.
    let phase = match query.phase.as_deref() {
        None | Some("") => None,
        Some("pending") => Some(nucleos::contract::ProcessPhase::Pending),
        Some("running") => Some(nucleos::contract::ProcessPhase::Running),
        Some("exited") => Some(nucleos::contract::ProcessPhase::Exited),
        Some("lost") => Some(nucleos::contract::ProcessPhase::Lost),
        Some(other) => {
            return Err(ApiError::from_error(&nucleos::Error::invalid(format!(
                "unknown phase {other:?}; expected pending, running, exited or lost"
            ))));
        }
    };

    let page = kernel
        .processes(&caller.0.tenant, &nucleos::contract::ProcessQuery {
            phase,
            limit: query.limit,
            after: query.page_token.map(nucleos::contract::SessionId::new),
        })
        .await
        .map_err(|error| ApiError::from_error(&error))?;

    Ok(Json(ProcessListResponse {
        processes: page
            .processes
            .into_iter()
            .map(|(session, process)| ProcessInfo {
                session: session.as_str().to_owned(),
                process,
            })
            .collect(),
        next_page_token: page.next.map(|next| next.as_str().to_owned()),
    }))
}

async fn write_stdin(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Json(request): Json<StdinRequest>,
) -> Result<StatusCode, ApiError> {
    let kernel = app.kernel()?.clone();
    let session = load(&kernel, &caller, &id).await?;
    kernel
        .write_stdin(&session, &request.chunk)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn signal_process(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let kernel = app.kernel()?.clone();
    let session = load(&kernel, &caller, &id).await?;
    kernel
        .signal(&session)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn wait_process(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<nucleos::contract::ExecOutput>, ApiError> {
    let kernel = app.kernel()?.clone();
    let session = load(&kernel, &caller, &id).await?;
    let output = kernel
        .wait(&session)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    Ok(Json(output))
}

/// Change what a session is doing, without closing it.
///
/// The wiring — kind, sandbox, workspace — is not reachable here on purpose: it is decided when the
/// session opens, and an application that could widen its own sandbox would be the end of the
/// sandbox.
async fn configure(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Json(patch): Json<nucleos::contract::ConfigPatch>,
) -> Result<Json<nucleos::wire::api::ConfigureResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let mut session = load(&kernel, &caller, &id).await?;
    let changed = kernel
        .configure(&mut session, patch)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    Ok(Json(nucleos::wire::api::ConfigureResponse {
        changed: changed.into_iter().map(str::to_owned).collect(),
    }))
}

async fn close(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<StartResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let mut session = load(&kernel, &caller, &id).await?;
    kernel
        .close(&mut session, nucleos::contract::StopReason::Cancelled)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    Ok(Json(StartResponse {
        session: session.id().as_str().to_owned(),
        state: SessionState::Closed,
        stop: Some(nucleos::contract::StopReason::Cancelled),
        next_seq: session.next_seq(),
    }))
}

async fn verdict(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Json(request): Json<VerdictRequest>,
) -> Result<Json<StartResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let outcome = agent::answer(
        &kernel,
        &caller.0,
        &SessionId::new(id),
        HitlVerdict {
            tool_call_id: request.tool_call_id,
            option_id: request.option_id,
        },
    )
    .await
    .map_err(|error| ApiError::from_error(&error))?;
    Ok(Json(StartResponse::from(&outcome)))
}

/// Query parameters for the event stream.
#[derive(Debug, serde::Deserialize)]
struct EventsQuery {
    /// The cursor to replay from, inclusive.
    #[serde(default)]
    from_seq: u64,
}

async fn events(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
    headers: axum::http::HeaderMap,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    let kernel = app.kernel()?.clone();

    // A reconnecting `EventSource` sends the last id it saw; that wins over the query, because a
    // client that reconnected did not mean to re-read what it already had.
    let from_seq = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(query.from_seq, |last| last.saturating_add(1));

    let events = kernel
        .events(&caller.0, &SessionId::new(id), from_seq)
        .await
        .map_err(|error| ApiError::from_error(&error))?;

    let stream = tokio_stream::iter(events).map(|event| {
        let frame = SseEvent::default()
            .event(event.kind.type_name())
            .id(event.seq.to_string())
            .data(serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_owned()));
        Ok(frame)
    });
    Ok(Sse::new(stream))
}

async fn load(
    kernel: &Kernel,
    caller: &Caller,
    id: &str,
) -> Result<nucleos::kernel::Session, ApiError> {
    kernel
        .load(&caller.0, &SessionId::new(id))
        .await
        .map_err(|error| ApiError::from_error(&error))
}

/// A failure, as the wire carries it.
pub struct ApiError {
    status: StatusCode,
    body: ErrorBody,
}

impl ApiError {
    fn from_error(error: &nucleos::Error) -> Self {
        let status = match error.code() {
            ErrorCode::Invalid => StatusCode::BAD_REQUEST,
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::Denied => StatusCode::FORBIDDEN,
            // A budget is not a rate limit, but 429 is the code clients already retry on, and the
            // body says which budget it was.
            ErrorCode::Budget => StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::Timeout => StatusCode::GATEWAY_TIMEOUT,
            ErrorCode::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            body: ErrorBody {
                code: error.code(),
                reason: error.reason().to_owned(),
                retryable: error.is_retryable(),
                retry_after_millis: (error.retry_after_millis() > 0)
                    .then(|| error.retry_after_millis()),
                details: error.details().clone(),
            },
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

/// The policy a deployment declared, or a refusal to start.
///
/// There is no default. The previous version of this function ran `AllowAll`, so every refusal this
/// daemon could report was a sentence in a document; and the version after that *claimed* to read a
/// file while still running `AllowAll`, which is worse. A daemon that cannot say what it refuses has
/// nothing to say, so it does not start.
///
/// Takes the path rather than reading the environment, so the decision is testable without a test
/// mutating process-wide state.
fn load_policy(path: Option<String>) -> Result<Arc<dyn nucleos::adapter::Policy>, nucleos::Error> {
    let Some(path) = path else {
        return Err(nucleos::Error::invalid(
            "NUCLEOS_POLICY is not set: name a policy file. Every call is gated by it, so there is \
             no safe default — see crates/nucleos-policy/examples/policy.json",
        ));
    };
    let ruleset = nucleos_policy::Ruleset::load(&path)?;
    // Said out loud at startup, because "which policy am I running under" is the first question
    // asked after an incident and the last one a log usually answers.
    tracing::info!(
        event = "policy_loaded",
        path = %path,
        default = ?ruleset.document().unmatched,
        rules = ruleset.document().rules.len(),
    );
    Ok(Arc::new(ruleset))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::load_policy;

    #[test]
    fn a_daemon_with_no_policy_refuses_to_start() {
        let Err(error) = load_policy(None) else {
            panic!("there is no default policy");
        };
        assert!(
            error.reason().contains("NUCLEOS_POLICY"),
            "the message has to name the variable: {error:?}"
        );
    }

    #[test]
    fn a_policy_file_is_loaded_and_an_unreadable_one_is_refused() {
        let example = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../nucleos-policy/examples/policy.json"
        );
        assert!(load_policy(Some(example.to_owned())).is_ok());
        assert!(load_policy(Some("/nonexistent/policy.json".to_owned())).is_err());
    }
}
