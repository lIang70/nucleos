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
use nucleos::kernel::{HitlVerdict, Kernel, SessionSpec, StateWrite};
use nucleos::wire::api::{
    DescribeResponse, ErrorBody, InferenceRequest, InferenceResponse, ProcessInfo,
    ProcessListResponse, ProcessStartRequest, ProcessStartResponse, SessionView, StartRequest,
    StartResponse, StateRequest, StdinRequest, ToolBatchRequest, ToolBatchResponse, VerdictRequest,
};
use nucleos::wire::openapi;
use nucleos_agent as agent;
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::StreamExt;

/// The ABI version this daemon speaks.
pub const ABI_VERSION: &str = "v1";

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
}

impl App {
    /// Serve a kernel.
    #[must_use]
    pub fn new(kernel: Arc<Kernel>, principal: Principal, models: Vec<String>) -> Self {
        Self {
            kernel: Some(kernel),
            principal,
            models,
        }
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
    /// ```
    ///
    /// Unset means unconfigured: up, describing itself, refusing to pretend it has models.
    pub fn from_env() -> Result<Self, nucleos::Error> {
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

        let routing = nucleos::contract::Routing {
            chain: vec![model.clone()],
            retries: 1,
        };
        let kernel = Kernel::builder()
            .model(&model, Arc::new(adapter))
            .store(Arc::new(nucleos::adapter::MemoryStore::new()))
            .policy(Arc::new(nucleos::adapter::AllowAll))
            .routing(routing)
            .build()?;

        Ok(Self::new(
            Arc::new(kernel),
            Self::unconfigured().principal,
            vec![model],
        ))
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
        .with_state(app)
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
    })
}

/// Open a session and run the reference loop to completion.
async fn run_agent(
    State(app): State<Arc<App>>,
    Json(request): Json<StartRequest>,
) -> Result<Json<StartResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let spec: SessionSpec = request.clone().into();
    let id = SessionId::new(request.session_id());
    let outcome = if request.resume {
        agent::resume(&kernel, &app.principal, &id).await
    } else {
        agent::start(&kernel, &app.principal, id, spec).await
    }
    .map_err(|error| ApiError::from_error(&error))?;
    Ok(Json(StartResponse::from(&outcome)))
}

/// Open a session without driving it.
async fn open(
    State(app): State<Arc<App>>,
    Json(request): Json<StartRequest>,
) -> Result<Json<StartResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let spec: SessionSpec = request.clone().into();
    let session = kernel
        .open(&app.principal, SessionId::new(request.session_id()), spec)
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
    Path(id): Path<String>,
) -> Result<Json<SessionView>, ApiError> {
    let kernel = app.kernel()?.clone();
    let session = load(&kernel, &app, &id).await?;
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
    }))
}

async fn inference(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(request): Json<InferenceRequest>,
) -> Result<Json<InferenceResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let mut session = load(&kernel, &app, &id).await?;
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
    Path(id): Path<String>,
    Json(request): Json<ToolBatchRequest>,
) -> Result<Json<ToolBatchResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let mut session = load(&kernel, &app, &id).await?;
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
    Path(id): Path<String>,
    Json(request): Json<StateRequest>,
) -> Result<StatusCode, ApiError> {
    let kernel = app.kernel()?.clone();
    let mut session = load(&kernel, &app, &id).await?;
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
    Json(request): Json<ProcessStartRequest>,
) -> Result<Json<ProcessStartResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
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
        .spawn(&app.principal, id, spec)
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
        .processes(&nucleos::contract::ProcessQuery {
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
    Path(id): Path<String>,
    Json(request): Json<StdinRequest>,
) -> Result<StatusCode, ApiError> {
    let kernel = app.kernel()?.clone();
    let session = load(&kernel, &app, &id).await?;
    kernel
        .write_stdin(&session, &request.chunk)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn signal_process(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let kernel = app.kernel()?.clone();
    let session = load(&kernel, &app, &id).await?;
    kernel
        .signal(&session)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn wait_process(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> Result<Json<nucleos::contract::ExecOutput>, ApiError> {
    let kernel = app.kernel()?.clone();
    let session = load(&kernel, &app, &id).await?;
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
    Path(id): Path<String>,
    Json(patch): Json<nucleos::contract::ConfigPatch>,
) -> Result<Json<nucleos::wire::api::ConfigureResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let mut session = load(&kernel, &app, &id).await?;
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
    Path(id): Path<String>,
) -> Result<Json<StartResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let mut session = load(&kernel, &app, &id).await?;
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
    Path(id): Path<String>,
    Json(request): Json<VerdictRequest>,
) -> Result<Json<StartResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let outcome = agent::answer(
        &kernel,
        &app.principal,
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
        .events(&SessionId::new(id), from_seq)
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

async fn load(kernel: &Kernel, app: &App, id: &str) -> Result<nucleos::kernel::Session, ApiError> {
    kernel
        .load(&app.principal, &SessionId::new(id))
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
