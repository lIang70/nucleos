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
use axum::routing::{MethodRouter, delete, get, post, put};
use axum::{Json, Router};
use nucleos::contract::{ErrorCode, Principal, SessionId, SessionState};
use nucleos_sqlite::SqliteStore;
use nucleos::kernel::{HitlVerdict, Kernel, SessionSpec, StateWrite};
use nucleos::wire::api::{
    DescribeResponse, ErrorBody, InferenceRequest, InferenceResponse, ProcessInfo,
    ConfigKey, ConfigResponse, MetricsResponse, ProcessListResponse, ProcessStartRequest, ProcessStartResponse, ReloadResponse,
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

/// Where `nucleos init` writes the policy the daemon enforces.
///
/// The same string as [`nucleos_cli::init::POLICY_KEY`], and duplicated rather than depended on: the
/// daemon must not link the CLI to learn a key name. The pair is held together by a test on each side
/// rather than by a shared import, which is the cheaper of the two ways to keep them in step.
pub const POLICY_KEY: &str = "config/policy";

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
                // Reachable when an administrative signature is presented to a route that wants a
                // bearer token: the middleware let the signature through for the `Admin` extractor,
                // and this route has none. Saying so beats a 500 for a caller who did something
                // reasonable with the wrong credential.
                ApiError::unauthorized(&nucleos::Error::denied(
                    "this route takes a bearer token, not an administrative signature",
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
    models: std::sync::RwLock<Vec<String>>,
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
    tokens: Option<crate::auth::TokenAuth>,
    /// Administrator signatures, when a system store holds keys to check them against.
    admins: Option<Arc<crate::admin::Verifier>>,
    /// The configuration database: what this daemon is, as opposed to what it is doing.
    system: Option<Arc<nucleos_sqlite::SystemStore>>,
    /// Where the data lives, reported by `GET /v1/config`.
    root: Option<std::path::PathBuf>,
}

impl App {
    /// Serve a kernel.
    #[must_use]
    pub fn new(kernel: Arc<Kernel>, principal: Principal, models: Vec<String>) -> Self {
        Self {
            kernel: Some(kernel),
            tokens: None,
            principal,
            models: std::sync::RwLock::new(models),
            tools: std::sync::RwLock::new(Vec::new()),
            source: None,
            admins: None,
            system: None,
            root: None,
            metrics: crate::metrics::Metrics::started_now(),
        }
    }

    /// The counters, for a handler and for a test.
    #[must_use]
    pub fn metrics(&self) -> &crate::metrics::Metrics {
        &self.metrics
    }

    /// Authenticate callers against the tokens a deployment has issued.
    #[must_use]
    pub fn with_tokens(mut self, tokens: crate::auth::TokenAuth) -> Self {
        self.tokens = Some(tokens);
        self
    }

    /// The authenticator, when this deployment has a token table.
    #[must_use]
    pub fn tokens(&self) -> Option<&crate::auth::TokenAuth> {
        self.tokens.as_ref()
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

    /// Replace the advertised model list, after a change that made it stale.
    ///
    /// `describe` is what a caller trusts before it opens a session; a list frozen at start-up would
    /// tell it the deployment has no models the moment after one was added.
    pub fn set_models(&self, models: Vec<String>) {
        if let Ok(mut current) = self.models.write() {
            *current = models;
        }
    }

    /// The models in force.
    fn models_now(&self) -> Vec<String> {
        self.models
            .read()
            .map(|models| models.clone())
            .unwrap_or_default()
    }

    /// The tools in force.
    fn tools_now(&self) -> Vec<String> {
        self.tools
            .read()
            .map(|tools| tools.clone())
            .unwrap_or_default()
    }

    /// Serve what the deployment has stored.
    ///
    /// The environment contributes nothing to this: models, policy and tokens are rows in the system
    /// store, written by `nucleos init` and by signed requests afterwards. What is left in the
    /// environment is the tool source (`NUCLEOS_MCP`) and the policy file fallback
    /// (`NUCLEOS_POLICY`), both scheduled for removal once their store-backed forms exist.
    pub async fn from_env(layout: &crate::layout::Layout) -> Result<Self, nucleos::Error> {
        // No environment variable configures a model. Models are rows in the system store, written
        // by `nucleos models add` through a signed request; a deployment with none starts, serves
        // `describe`, and refuses to open an agent session until somebody adds one.
        let system = Arc::new(nucleos_sqlite::SystemStore::open(layout.system_store())?);
        tracing::debug!(
            event = "system_store_open",
            path = %layout.system_store().display(),
            schema = system.schema_version()?,
        );
        let stored = crate::configure::stored_models(&system)?;

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
        let store = SqliteStore::open(layout.session_store())?;
        // Tokens come from the store, not from the environment: a deployment's callers are state, and
        // state has one writer. A deployment with none is the socket-permissions case, which the
        // authenticator answers by itself rather than by a variable somebody sets.
        let tokens = crate::auth::TokenAuth::new(
            Arc::clone(&system),
            Self::unconfigured().principal,
        );

        let policy = match system.get(POLICY_KEY)? {
            Some(text) => {
                let document = nucleos_policy::Document::parse(&text)?;
                tracing::info!(
                    event = "policy_loaded",
                    source = "system store",
                    default = ?document.unmatched,
                    rules = document.rules.len(),
                );
                Arc::new(nucleos_policy::Ruleset::new(document)?) as Arc<dyn nucleos::adapter::Policy>
            }
            None => {
                return Err(nucleos::Error::invalid(format!(
                    "{} has no policy: run `nucleos init` to write the bootstrap one",
                    layout.root.display()
                )));
            }
        };
        // The routing a session inherits names the first stored model: a session that does not
        // choose one still has somewhere to go, and a deployment with no models has an empty chain
        // that fails by saying so.
        let routing = nucleos::contract::Routing {
            chain: stored.first().map(|model| model.name.clone()).into_iter().collect(),
            retries: 1,
        };
        let mut builder = Kernel::builder()
            .store(Arc::new(store))
            .policy(policy)
            .routing(routing);
        for model in &stored {
            let adapter = crate::configure::adapter_for(&nucleos::wire::api::ModelRequest {
                name: model.name.clone(),
                url: model.url.clone(),
                model: model.model.clone(),
                key_file: model.key_file.clone(),
                context_window: Some(model.context_window),
                strict_tools: model.strict_tools,
                reasoning: model.reasoning,
            })?;
            builder = builder.model(&model.name, Arc::new(adapter));
        }
        for tool in tools {
            builder = builder.tool(tool);
        }
        let kernel = builder.build()?;

        let models: Vec<String> = stored.into_iter().map(|model| model.name).collect();

        let app = Self::new(Arc::new(kernel), Self::unconfigured().principal, models)
        .with_system(
            system,
            layout.root.clone(),
            Arc::new(nucleos::adapter::SystemClock),
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
            models: std::sync::RwLock::new(Vec::new()),
            tools: std::sync::RwLock::new(Vec::new()),
            source: None,
            tokens: None,
            admins: None,
            system: None,
            root: None,
            metrics: crate::metrics::Metrics::started_now(),
        }
    }

    /// Give the daemon a system store: `GET /v1/config` reads it, and administrator signatures are
    /// checked against the keys inside it.
    #[must_use]
    pub fn with_system(
        mut self,
        system: Arc<nucleos_sqlite::SystemStore>,
        root: std::path::PathBuf,
        clock: Arc<dyn nucleos::adapter::Clock>,
    ) -> Self {
        self.admins = Some(Arc::new(crate::admin::Verifier::new(
            Arc::clone(&system),
            clock,
        )));
        self.system = Some(system);
        self.root = Some(root);
        self
    }

    /// The signature verifier, when this deployment has keys to check against.
    #[must_use]
    pub fn admins(&self) -> Option<&Arc<crate::admin::Verifier>> {
        self.admins.as_ref()
    }

    /// The configuration database.
    #[must_use]
    pub fn system(&self) -> Option<&Arc<nucleos_sqlite::SystemStore>> {
        self.system.as_ref()
    }

    /// Where the data lives.
    #[must_use]
    pub fn root(&self) -> Option<&std::path::Path> {
        self.root.as_deref()
    }

    pub(crate) fn kernel(&self) -> Result<&Arc<Kernel>, ApiError> {
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
            method: "put",
            path: "/v1/policy",
            operation: "putPolicy",
            summary: "Replace the policy in force. Signed, gated, and recorded.",
            request: None,
            reply: openapi::Reply::Json("PutResponse"),
        },
        build: || put(crate::configure::put_policy),
    },
    Route {
        meta: openapi::Route {
            method: "post",
            path: "/v1/tokens",
            operation: "issueToken",
            summary: "Issue a tenant token. The value is returned once.",
            request: Some("TokenIssueRequest"),
            reply: openapi::Reply::Json("TokenIssued"),
        },
        build: || post(crate::configure::post_token),
    },
    Route {
        meta: openapi::Route {
            method: "get",
            path: "/v1/tokens",
            operation: "listTokens",
            summary: "The tokens this deployment has issued, without their values.",
            request: None,
            reply: openapi::Reply::Json("TokenListResponse"),
        },
        build: || get(crate::configure::list_tokens),
    },
    Route {
        meta: openapi::Route {
            method: "delete",
            path: "/v1/tokens/{id}",
            operation: "revokeToken",
            summary: "Revoke a token. Effective on the next request.",
            request: None,
            reply: openapi::Reply::Json("TokenListResponse"),
        },
        build: || delete(crate::configure::delete_token),
    },
    Route {
        meta: openapi::Route {
            method: "get",
            path: "/v1/models",
            operation: "listModels",
            summary: "The models this deployment has.",
            request: None,
            reply: openapi::Reply::Json("ModelListResponse"),
        },
        build: || get(crate::configure::list_models),
    },
    Route {
        meta: openapi::Route {
            method: "post",
            path: "/v1/models",
            operation: "addModel",
            summary: "Add or replace a model. Signed, gated, and recorded.",
            request: Some("ModelRequest"),
            reply: openapi::Reply::Json("ModelView"),
        },
        build: || post(crate::configure::post_model),
    },
    Route {
        meta: openapi::Route {
            method: "delete",
            path: "/v1/models/{name}",
            operation: "removeModel",
            summary: "Remove a model.",
            request: None,
            reply: openapi::Reply::Json("ModelListResponse"),
        },
        build: || delete(crate::configure::delete_model),
    },
    Route {
        meta: openapi::Route {
            method: "get",
            path: "/v1/config",
            operation: "config",
            summary: "What this daemon is running, for an administrator who signed the request.",
            request: None,
            reply: openapi::Reply::Json("ConfigResponse"),
        },
        build: || get(config),
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
    // Two kinds of credential reach this daemon, and the middleware is admission for only one of
    // them: an administrative signature is verified by the `Admin` extractor, which needs the body.
    // Passing it through here is not a bypass — that route's handler refuses a request the signature
    // does not cover, and a tenant route refuses it for having no bearer token.
    if header.is_some_and(|value| value.trim_start().starts_with("Nucleos-Admin")) {
        return next.run(request).await;
    }

    // No token table means no token exists to check: the socket's permissions are the
    // authentication, and every call is attributed to the daemon's own principal.
    let resolved = match app.tokens() {
        Some(tokens) => tokens.authenticate(header),
        None => Ok(app.principal.clone()),
    };
    match resolved {
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
        models: app.models_now(),
        tools: app.tools_now(),
        generation: app.kernel.as_ref().map(|kernel| kernel.generation().0).unwrap_or(0),
    })
}

/// What this daemon is running.
///
/// The first signed endpoint, and deliberately a read: the mechanism that proves *who* is asking comes
/// before the surface that changes anything. An administrator can confirm what the daemon is enforcing
/// — which policy, which keys, which generation — without a mutation existing yet.
async fn config(
    State(app): State<Arc<App>>,
    admin: crate::admin::Admin,
) -> Result<Json<ConfigResponse>, ApiError> {
    let Some(system) = app.system() else {
        return Err(ApiError::from_error(&nucleos::Error::unavailable(
            "this daemon has no system store",
        )));
    };

    let policy = system
        .get(POLICY_KEY)
        .map_err(|error| ApiError::from_error(&error))?;
    let keys = system
        .get(crate::admin::ADMIN_KEYS_KEY)
        .map_err(|error| ApiError::from_error(&error))?;
    let keys: Vec<nucleos_auth::PublicKey> = keys
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| {
            ApiError::from_error(&nucleos::Error::internal(format!(
                "stored administrator keys are unreadable: {error}"
            )))
        })?
        .unwrap_or_default();

    tracing::info!(
        event = "config_read",
        by = %admin.principal.subject,
        tenant = %admin.principal.tenant,
    );

    Ok(Json(ConfigResponse {
        schema_version: system
            .schema_version()
            .map_err(|error| ApiError::from_error(&error))?,
        root: app.root().map(|root| root.display().to_string()),
        policy_json: policy,
        admin_keys: keys
            .into_iter()
            .map(|key| ConfigKey {
                id: key.id,
                tenant: key.tenant,
                subject: key.subject,
            })
            .collect(),
        generation: app
            .kernel
            .as_ref()
            .map(|kernel| kernel.generation().0)
            .unwrap_or(0),
        // The body is what the signature covered. Reporting its length is the cheapest way for a
        // caller to see that verification ran over the request they actually sent.
        signed_body_bytes: admin.body.len() as u64,
    }))
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
    /// A request whose credentials were missing or wrong.
    ///
    /// `401`, not `403`: the difference is "who are you" versus "not you". A denied policy verdict is
    /// the second; an unsigned or badly signed request is the first, and a client that sees `403` for a
    /// missing credential will look for a permission it does not lack.
    pub(crate) fn unauthorized(error: &nucleos::Error) -> Self {
        let mut refused = Self::from_error(error);
        refused.status = StatusCode::UNAUTHORIZED;
        refused
    }

    pub(crate) fn from_error(error: &nucleos::Error) -> Self {
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use nucleos_sqlite::SystemStore;

    /// A deployment with no policy in its store cannot start, and says what to run.
    ///
    /// The policy used to come from a file named by an environment variable. That is gone: one source
    /// for the rules a daemon enforces, and a deployment missing it is uninitialised rather than
    /// unconfigured.
    #[tokio::test]
    async fn a_store_without_a_policy_is_not_a_deployment() {
        let root = std::env::temp_dir().join(format!("nucleos-nopolicy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let layout = crate::layout::Layout::resolve(Some(root.clone()), None);

        // A store that exists and is empty: `init` has not run.
        SystemStore::open(layout.system_store()).unwrap();
        let Err(error) = crate::http::App::from_env(&layout).await else {
            panic!("a store without a policy must not start");
        };
        assert!(error.reason().contains("nucleos init"), "{error:?}");

        let _ = std::fs::remove_dir_all(&root);
    }
}
