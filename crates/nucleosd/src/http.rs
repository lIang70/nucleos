//! The HTTP surface: a thin translation of the kernel onto the wire.
//!
//! Nothing here decides anything. It parses a request into kernel types, calls the kernel, and
//! serializes what came back — which is why no handler contains a branch that changes behaviour.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use nucleos::contract::{ErrorCode, Principal, SessionId};
use nucleos::engine::{HitlVerdict, Kernel, SessionSpec};
use nucleos::wire::api::{
    DescribeResponse, ErrorBody, StartRequest, StartResponse, VerdictRequest,
};
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
    /// daemon can honestly claim; deriving a principal from them is the next slice, and inventing
    /// a token scheme before that would be a promise it cannot keep.
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

/// The router.
pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/v1/describe", get(describe))
        .route("/v1/sessions", post(start))
        .route("/v1/sessions/{id}/verdict", post(verdict))
        .route("/v1/sessions/{id}/events", get(events))
        .with_state(app)
}

async fn describe(State(app): State<Arc<App>>) -> Json<DescribeResponse> {
    Json(DescribeResponse {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        abi_version: ABI_VERSION.to_owned(),
        models: app.models.clone(),
    })
}

async fn start(
    State(app): State<Arc<App>>,
    Json(request): Json<StartRequest>,
) -> Result<Json<StartResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let spec: SessionSpec = request.clone().into();
    let id = SessionId::new(request.session_id());
    let outcome = if request.resume {
        kernel.resume(&app.principal, id, spec).await
    } else {
        kernel.start(&app.principal, id, spec).await
    }
    .map_err(|error| ApiError::from_error(&error))?;
    Ok(Json(StartResponse::from(&outcome)))
}

async fn verdict(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(request): Json<VerdictRequest>,
) -> Result<Json<StartResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let outcome = kernel
        .answer(
            &app.principal,
            SessionId::new(id),
            SessionSpec::default(),
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
