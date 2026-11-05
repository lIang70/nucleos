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
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, delete, get, post, put};
use axum::{Json, Router};
use nucleos::contract::{ErrorCode, Event, EventKind, Principal, SessionId, SessionState};
use nucleos::kernel::{Kernel, SessionSpec, StateWrite};
use nucleos::wire::api::{
    AnswerRequest, AppliedDeployment, CancelResponse, ChangeListResponse, ChangeView, ConfigKey,
    ConfigResponse, CreditRequest, CreditResponse, DeletedResponse, Deployment, DescribeResponse,
    ErrorBody, InferenceRequest, InferenceResponse, MetricsResponse, ProcessInfo,
    ProcessListResponse, ProcessStartRequest, ProcessStartResponse, ReloadResponse, RetireResponse,
    SessionListResponse, SessionSummary, SessionView, StartRequest, StartResponse, StateRequest,
    StdinRequest, ToolBatchRequest, ToolBatchResponse,
};
use nucleos::wire::openapi::{self, Kind, Parameter};
use nucleos_sqlite::SqliteStore;
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;

/// The ABI version this daemon speaks.
pub const ABI_VERSION: &str = "v1";

/// Where `nucleos init` writes the policy the daemon enforces.
///
/// The same string as `nucleos_cli::init::POLICY_KEY`, and duplicated rather than depended on: the
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
    /// The MCP servers this deployment runs.
    ///
    /// Behind a lock because a source is added by request: the set is state, and the tool generation it
    /// produces is derived from it.
    sources: tokio::sync::RwLock<crate::sources::SourceSet>,
    /// What this daemon has done since it started.
    metrics: crate::metrics::Metrics,
    /// Counts requests, for the id a log line and a response header share.
    requests: std::sync::atomic::AtomicU64,
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
    /// The key that seals credentials stored in the system store, when the deployment has one.
    ///
    /// Absent means the daemon cannot store an API key at all — it will not keep one in the clear, and a
    /// built-in default would be a constant dressed as encryption.
    seal: Option<nucleos_auth::SealKey>,
    /// Where the data lives, reported by `GET /v1/config`.
    root: Option<std::path::PathBuf>,
    /// The session store, for a listing that must not go through a conversation.
    ///
    /// Optional because `App::new` is what every test uses, and only the listing needs it: a required
    /// parameter would have changed every construction site for one endpoint.
    store: Option<Arc<dyn nucleos::adapter::Store>>,
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
            store: None,
            tools: std::sync::RwLock::new(Vec::new()),
            sources: tokio::sync::RwLock::new(crate::sources::SourceSet::default()),
            admins: None,
            system: None,
            seal: None,
            root: None,
            metrics: crate::metrics::Metrics::started_now(),
            requests: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The counters, for a handler and for a test.
    #[must_use]
    pub fn metrics(&self) -> &crate::metrics::Metrics {
        &self.metrics
    }

    /// The next request id: monotonic, cheap, and only ever compared with itself.
    ///
    /// Not a UUID. What it has to do is pick out one line in a log and match the header the caller saw,
    /// and a counter does that with no dependency, no randomness and no pretence of being a secret. The
    /// process id is folded in so two daemons' logs can be interleaved without ambiguity.
    pub(crate) fn next_request_id(&self) -> String {
        let sequence = self
            .requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("{:x}{:04x}", std::process::id() as u16, sequence & 0xffff)
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

    /// Adopt the sources this deployment runs.
    #[must_use]
    pub fn with_sources(self, sources: crate::sources::SourceSet) -> Self {
        // Contended only if another task is already using the set, which at construction it is not.
        if let Ok(mut current) = self.sources.try_write() {
            *current = sources;
        }
        self
    }

    /// The sources. `tokio`'s lock rather than `std`'s: adding a source connects a process, so the
    /// guard is held across an `await`, and a `std` guard there would make every handler's future
    /// non-`Send`.
    #[must_use]
    pub fn sources(&self) -> &tokio::sync::RwLock<crate::sources::SourceSet> {
        &self.sources
    }

    /// Replace the advertised tool list, after a generation was published.
    pub fn set_tools(&self, tools: Vec<String>) {
        if let Ok(mut current) = self.tools.write() {
            *current = tools;
        }
    }

    /// Advertise the tools that were discovered, so a caller can see what it may ask for.
    #[must_use]
    pub fn with_tools(self, tools: Vec<String>) -> Self {
        if let Ok(mut current) = self.tools.write() {
            *current = tools;
        }
        self
    }

    /// Give the daemon the store its listings read.
    #[must_use]
    pub fn with_store(mut self, store: Arc<dyn nucleos::adapter::Store>) -> Self {
        self.store = Some(store);
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

    /// Load a deployment: both stores, the policy, the sources, the tokens.
    ///
    /// Named for what it does. It used to be `from_env`, and the name was true once — the environment
    /// configured the models, the policy and the tokens. It configures nothing now, and a method whose
    /// name describes a source it no longer reads is the kind of comment this project deletes.
    ///
    /// A store without a policy is not a deployment: `nucleos init` has not run, and the daemon says so
    /// rather than serving a refusal it cannot explain.
    pub async fn load(layout: &crate::layout::Layout) -> Result<Self, nucleos::Error> {
        // No environment variable configures a model. Models are rows in the system store, written
        // by `nucleos models add` through a signed request; a deployment with none starts, serves
        // `describe`, and refuses to open an agent session until somebody adds one.
        let system = Arc::new(nucleos_sqlite::SystemStore::open(layout.system_store())?);
        tracing::debug!(
            event = "system_store_open",
            path = %layout.system_store().display(),
            schema = system.schema_version().await?,
        );
        let stored = crate::configure::stored_models(&system).await?;
        // Resolved before any adapter is built: a sealed key has to be openable at start-up, and a
        // missing seal key with sealed rows in the store is a deployment that cannot reach its models.
        let seal = nucleos_auth::SealKey::resolve(&layout.seal_key)?;
        // Said at start-up, because "which one am I actually using" is the question that follows moving
        // the key to a stronger store, and a daemon still reading the old file looks identical to one
        // that read the new credential.
        match &seal {
            Some(resolved) => tracing::info!(
                event = "seal_key_loaded",
                source = %resolved.source.describe(),
                kind = match resolved.source {
                    nucleos_auth::seal::Source::Credential(_) => "credential",
                    nucleos_auth::seal::Source::Keychain { .. } => "keychain",
                    nucleos_auth::seal::Source::File(_) => "file",
                },
            ),
            None => tracing::info!(
                event = "seal_key_absent",
                consequence = "a model API key cannot be stored; pass a key_file instead, or create a \
                               seal key with `nucleos init`",
            ),
        }
        let seal = seal.map(|resolved| resolved.key);

        // Sources are rows in the system store, written by `nucleos sources add`. What the daemon
        // connects here is what a previous run was told to run; a server that will not start is a
        // warning and a missing tool, not a refusal to boot.
        let mut tools: Vec<Arc<dyn nucleos::adapter::Tool>> = Vec::new();
        let mut discovered: Vec<String> = Vec::new();

        // Two files with two lifecycles: configuration is versioned and kept, sessions are pruned.
        // Both are named inside one root, so a second instance on this host is one more `--root`.
        layout.create().map_err(|error| {
            nucleos::Error::unavailable(format!("cannot create {}: {error}", layout.root.display()))
        })?;
        let store: Arc<dyn nucleos::adapter::Store> =
            Arc::new(SqliteStore::open(layout.session_store())?);
        let sources = crate::sources::SourceSet::load(&system, seal.as_ref()).await?;
        for source in sources.views() {
            tracing::info!(
                event = "source_loaded",
                source = %source.name,
                tools = source.tools.len(),
            );
            discovered.extend(source.tools);
        }
        // Tokens come from the store, not from the environment: a deployment's callers are state, and
        // state has one writer. A deployment with none is the socket-permissions case, which the
        // authenticator answers by itself rather than by a variable somebody sets.
        let tokens =
            crate::auth::TokenAuth::new(Arc::clone(&system), Self::unconfigured().principal);

        // The deployment's policy, composed with one per tenant that wrote its own. Composed rather
        // than replaced: a tenant that wrote an empty document would otherwise allow everything the
        // deployment forbids.
        let layered = crate::configure::layered_from(&system).await?;
        tracing::info!(
            event = "policy_loaded",
            source = "system store",
            tenants = ?layered.tenants(),
        );
        let policy: Arc<dyn nucleos::adapter::Policy> = Arc::new(layered);
        // The routing a session inherits names the first stored model: a session that does not
        // choose one still has somewhere to go, and a deployment with no models has an empty chain
        // that fails by saying so.
        let routing = nucleos::contract::Routing {
            chain: stored
                .first()
                .map(|model| model.name.clone())
                .into_iter()
                .collect(),
            retries: 1,
        };
        let mut builder = Kernel::builder()
            .store(Arc::clone(&store))
            .policy(policy)
            .routing(routing);
        for model in &stored {
            let api_key = match &seal {
                Some(seal) => match system
                    .get(&format!(
                        "{}{}",
                        crate::configure::SEALED_KEY_PREFIX,
                        model.name
                    ))
                    .await?
                {
                    Some(sealed) => Some(seal.open(&sealed)?),
                    None => None,
                },
                None => None,
            };
            let adapter = crate::configure::adapter_for(
                &nucleos::wire::api::ModelRequest {
                    name: model.name.clone(),
                    url: model.url.clone(),
                    model: model.model.clone(),
                    key_file: model.key_file.clone(),
                    context_window: Some(model.context_window),
                    strict_tools: model.strict_tools,
                    reasoning: model.reasoning,
                    ..Default::default()
                },
                api_key,
            )?;
            builder = builder.model(&model.name, Arc::new(adapter));
        }
        for tool in sources.tools() {
            tools.push(tool);
        }
        for tool in tools {
            builder = builder.tool(tool);
        }
        let kernel = builder.build()?;
        // Limits a previous run stored are applied before the first request: a ceiling an operator set
        // during an incident has to survive the restart that incident caused.
        crate::configure::apply_tenant_limits(&kernel, &system).await?;

        let models: Vec<String> = stored.into_iter().map(|model| model.name).collect();

        let app = Self::new(Arc::new(kernel), Self::unconfigured().principal, models)
            .with_system(
                system,
                layout.root.clone(),
                Arc::new(nucleos::adapter::SystemClock),
            )
            .with_sources(sources)
            .with_store(store)
            .with_seal(seal)
            .with_tools(discovered)
            .with_tokens(tokens);
        Ok(app)
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
            store: None,
            tools: std::sync::RwLock::new(Vec::new()),
            sources: tokio::sync::RwLock::new(crate::sources::SourceSet::default()),
            tokens: None,
            admins: None,
            system: None,
            seal: None,
            root: None,
            metrics: crate::metrics::Metrics::started_now(),
            requests: std::sync::atomic::AtomicU64::new(0),
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

    /// Adopt the key that seals stored credentials.
    #[must_use]
    pub fn with_seal(mut self, seal: Option<nucleos_auth::SealKey>) -> Self {
        self.seal = seal;
        self
    }

    /// The sealing key, when the deployment has one.
    #[must_use]
    pub fn seal(&self) -> Option<&nucleos_auth::SealKey> {
        self.seal.as_ref()
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
            parameters: &[],
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
            parameters: &[],
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
            parameters: &[],
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
            parameters: &[],
            method: "post",
            path: "/v1/sessions/{id}/inference",
            operation: "inference",
            summary: "One model call. `Accept: text/event-stream` streams the turn's events; the \
                      last frame is the assembled message.",
            request: Some("InferenceRequest"),
            reply: openapi::Reply::JsonOrStream("InferenceResponse"),
        },
        build: || post(inference),
    },
    Route {
        meta: openapi::Route {
            parameters: &[],
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
            parameters: &[],
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
            parameters: &[],
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
            parameters: &[],
            method: "delete",
            path: "/v1/sessions/{id}",
            operation: "deleteSession",
            summary: "Remove a closed session and its log. An open session answers 409.",
            request: None,
            reply: openapi::Reply::Json("DeletedResponse"),
        },
        build: || axum::routing::delete(delete_session),
    },
    Route {
        meta: openapi::Route {
            parameters: &[],
            method: "post",
            path: "/v1/sessions/{id}/cancel",
            operation: "cancelTurn",
            summary: "Stop the turn this session is running. The session stays open.",
            request: None,
            reply: openapi::Reply::Json("CancelResponse"),
        },
        build: || post(cancel_turn),
    },
    Route {
        meta: openapi::Route {
            method: "get",
            path: "/v1/sessions",
            operation: "listSessions",
            summary: "The sessions this caller owns, newest page first.",
            request: None,
            reply: openapi::Reply::Json("SessionListResponse"),
            parameters: &[
                Parameter::query(
                    "kind",
                    Kind::String,
                    "Only sessions of this kind: `agent` or `process`.",
                ),
                Parameter::query("limit", Kind::Integer, "How many rows; 20 when absent."),
                Parameter::query(
                    "page_token",
                    Kind::String,
                    "Continue after this session id.",
                ),
            ],
        },
        build: || get(list_sessions),
    },
    Route {
        meta: openapi::Route {
            method: "get",
            path: "/v1/processes",
            operation: "listProcesses",
            summary: "The processes this node is running. Not durable: memory, not the log.",
            request: None,
            reply: openapi::Reply::Json("ProcessListResponse"),
            parameters: &[
                Parameter::query("page_size", Kind::Integer, "How many rows; 20 when absent."),
                Parameter::query(
                    "page_token",
                    Kind::String,
                    "Continue after this session id.",
                ),
            ],
        },
        build: || get(list_processes),
    },
    Route {
        meta: openapi::Route {
            parameters: &[],
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
            parameters: &[],
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
            parameters: &[],
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
            parameters: &[],
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
            parameters: &[],
            method: "put",
            path: "/v1/tenants/{tenant}/limits",
            operation: "putTenantLimits",
            summary: "Set what one tenant may use: a tool ceiling and a model allow-list.",
            request: Some("TenantLimitsRequest"),
            reply: openapi::Reply::Json("TenantLimitsView"),
        },
        build: || put(crate::configure::put_tenant_limits),
    },
    Route {
        meta: openapi::Route {
            parameters: &[],
            method: "put",
            path: "/v1/tenants/{tenant}/policy",
            operation: "putTenantPolicy",
            summary: "Replace one tenant's policy. It can narrow the deployment's, never widen it.",
            request: None,
            reply: openapi::Reply::Json("PutResponse"),
        },
        build: || put(crate::configure::put_tenant_policy),
    },
    Route {
        meta: openapi::Route {
            parameters: &[],
            method: "get",
            path: "/v1/admin/keys",
            operation: "listAdminKeys",
            summary: "The administrators this deployment trusts.",
            request: None,
            reply: openapi::Reply::Json("AdminKeyListResponse"),
        },
        build: || get(crate::configure::list_admin_keys),
    },
    Route {
        meta: openapi::Route {
            parameters: &[],
            method: "post",
            path: "/v1/admin/keys",
            operation: "enrolAdminKey",
            summary: "Enrol another administrator's public key.",
            request: Some("AdminKeyRequest"),
            reply: openapi::Reply::Json("AdminKeyListResponse"),
        },
        build: || post(crate::configure::post_admin_key),
    },
    Route {
        meta: openapi::Route {
            parameters: &[],
            method: "delete",
            path: "/v1/admin/keys/{id}",
            operation: "revokeAdminKey",
            summary: "Revoke an administrator's key. Effective on the next request.",
            request: None,
            reply: openapi::Reply::Json("AdminKeyListResponse"),
        },
        build: || delete(crate::configure::delete_admin_key),
    },
    Route {
        meta: openapi::Route {
            parameters: &[],
            method: "get",
            path: "/v1/sources",
            operation: "listSources",
            summary: "The MCP servers this deployment runs, and the tools each contributes.",
            request: None,
            reply: openapi::Reply::Json("SourceListResponse"),
        },
        build: || get(crate::sources::list_sources),
    },
    Route {
        meta: openapi::Route {
            parameters: &[],
            method: "post",
            path: "/v1/sources",
            operation: "addSource",
            summary: "Add or replace a tool source, publishing a new generation.",
            request: Some("SourceRequest"),
            reply: openapi::Reply::Json("SourceView"),
        },
        build: || post(crate::sources::post_source),
    },
    Route {
        meta: openapi::Route {
            parameters: &[],
            method: "delete",
            path: "/v1/sources/{name}",
            operation: "removeSource",
            summary: "Remove a tool source, publishing a new generation without its tools.",
            request: None,
            reply: openapi::Reply::Json("SourceListResponse"),
        },
        build: || delete(crate::sources::delete_source),
    },
    Route {
        meta: openapi::Route {
            parameters: &[],
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
            parameters: &[],
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
            parameters: &[],
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
            parameters: &[],
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
            parameters: &[],
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
            parameters: &[],
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
            parameters: &[],
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
            parameters: &[],
            method: "put",
            path: "/v1/config",
            operation: "applyConfig",
            summary: "Replace the whole configuration from one document. `If-Match` makes it a \
                      compare-and-swap.",
            request: Some("Deployment"),
            reply: openapi::Reply::Json("AppliedDeployment"),
        },
        build: || put(apply_config),
    },
    Route {
        meta: openapi::Route {
            method: "get",
            path: "/v1/config/history",
            operation: "configHistory",
            summary: "Configuration changes, newest first, with the key that signed each one.",
            request: None,
            reply: openapi::Reply::Json("ChangeListResponse"),
            parameters: &[Parameter::query(
                "limit",
                Kind::Integer,
                "How many rows, newest first; 50 when absent.",
            )],
        },
        build: || get(config_history),
    },
    Route {
        meta: openapi::Route {
            parameters: &[],
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
            parameters: &[],
            method: "post",
            path: "/v1/tools/retire",
            operation: "retireGenerations",
            summary: "Forget every tool set but the current one. Running sessions are unaffected.",
            request: None,
            reply: openapi::Reply::Json("RetireResponse"),
        },
        build: || post(retire),
    },
    Route {
        meta: openapi::Route {
            parameters: &[],
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
            parameters: &[],
            method: "post",
            path: "/v1/sessions/prune",
            operation: "pruneSessions",
            summary: "Remove closed sessions idle past a deadline. `dry_run` defaults to true.",
            request: Some("PruneRequest"),
            reply: openapi::Reply::Json("PruneResponse"),
        },
        build: || post(crate::retention::prune),
    },
    Route {
        meta: openapi::Route {
            parameters: &[],
            method: "post",
            path: "/v1/sessions/{id}/credit",
            operation: "creditSession",
            summary: "Grant more credit, and revive a session that stopped on a budget.",
            request: Some("CreditRequest"),
            reply: openapi::Reply::Json("CreditResponse"),
        },
        build: || post(credit),
    },
    Route {
        meta: openapi::Route {
            parameters: &[],
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
            parameters: &[],
            method: "post",
            path: "/v1/sessions/{id}/answer",
            operation: "answerPending",
            summary: "Answer the question a session is parked on: a tool call, or a command.",
            request: Some("AnswerRequest"),
            reply: openapi::Reply::Json("StartResponse"),
        },
        build: || post(answer),
    },
    Route {
        meta: openapi::Route {
            parameters: &[],
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
            method: "get",
            path: "/v1/sessions/{id}/events",
            operation: "events",
            summary: "The event stream. `id:` is the event's `seq`, so Last-Event-ID resumes.",
            request: None,
            reply: openapi::Reply::Stream,
            parameters: &[
                Parameter::query(
                    "from_seq",
                    Kind::Integer,
                    "The cursor to replay from, inclusive. Absent replays the whole log.",
                ),
                Parameter::header(
                    "Last-Event-ID",
                    Kind::String,
                    "The last `id` this client saw; wins over `from_seq`.",
                ),
            ],
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
    // The id a caller quotes when they report a problem. Generated here, outside authentication, so a
    // refused request has one too — the refusals are exactly the ones nobody can reproduce later.
    let request_id = app.next_request_id();
    // Which session the path names, when it names one. Read from the URI rather than from the handler,
    // because a correlation a handler has to remember to set is a correlation missing from the route
    // somebody added in a hurry.
    let session = session_in(request.uri().path());
    // The template, not the path: `/v1/sessions/{id}` is one route, however many sessions exist.
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|path| path.as_str().to_owned())
        // A route that matched nothing is one bucket, not one per guess. `Router::layer` runs on
        // unmatched paths, so `GET /v1/<random>` from an unauthenticated caller used to add a key to
        // the map for every request it made.
        .unwrap_or_else(|| "unmatched".to_owned());

    let mut response = next.run(request).await;
    let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    app.metrics
        .observe(&method, &route, response.status().as_u16());

    // Who, if authentication got that far. `authenticate` puts the principal on the response so this
    // layer — which wraps it — can see it after the fact.
    let (tenant, subject) = response.extensions().get::<Principal>().map_or_else(
        || ("-".to_owned(), "-".to_owned()),
        |principal| (principal.tenant.clone(), principal.subject.clone()),
    );

    tracing::info!(
        kind = "request",
        request_id = %request_id,
        method = %method,
        route = %route,
        status = response.status().as_u16(),
        duration_millis = millis,
        tenant = %tenant,
        subject = %subject,
        session = session.as_deref().unwrap_or("-"),
    );

    // The same id on the wire: a client that reports "the inference at 14:02 failed" can quote
    // something that matches exactly one line.
    if let Ok(value) = request_id.parse() {
        response.headers_mut().insert("nucleos-request-id", value);
    }
    response
}

/// The session a path names, if it names one.
///
/// `/v1/sessions/{id}` and everything under it; nothing else. Deliberately a prefix test rather than a
/// route match: the route template is `/v1/sessions/{id}/inference`, and what an operator wants is the
/// id, not the template.
fn session_in(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/v1/sessions/")?;
    let id = rest.split('/').next().unwrap_or("");
    (!id.is_empty()).then(|| id.to_owned())
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
    //
    // Passing it through here is safe **only because every route extracts one of the two**: a tenant
    // route answers 401 through `Caller` (no principal was installed), and an admin route verifies the
    // signature itself. A route that extracted neither would be reachable by anyone who typed this
    // header, which is what `describe`, `metrics`, `reload` and `retire` were until they were given the
    // guard the rest of the table had. `every_published_route_refuses_a_signature_that_verifies_nothing`
    // is the test that keeps that sentence true.
    if header.is_some_and(|value| value.trim_start().starts_with("Nucleos-Admin")) {
        return next.run(request).await;
    }

    // No token table means no token exists to check: the socket's permissions are the
    // authentication, and every call is attributed to the daemon's own principal.
    let resolved = match app.tokens() {
        Some(tokens) => tokens.authenticate(header).await,
        None => Ok(app.principal.clone()),
    };
    match resolved {
        Ok(principal) => {
            // The identity travels with the request: a handler that read it from the app would be
            // reading the daemon's, which is the same only when there is one caller.
            //
            // It is copied onto the response as well, because the layer that writes the request line
            // wraps this one and cannot see the request's extensions after `next` has run.
            let mut request = request;
            request.extensions_mut().insert(principal.clone());
            let mut response = next.run(request).await;
            // Copied out, not borrowed: the request is gone by the time the outer layer looks.
            response.extensions_mut().insert(principal);
            response
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

async fn describe(caller: Caller, State(app): State<Arc<App>>) -> Json<DescribeResponse> {
    // It reads the model and tool inventory, so it is a tenant read and not a public one — which is
    // also what makes it safe for the middleware to pass an administrative signature through: this
    // extractor answers 401 for a request that has no principal, and a signature is not one.
    let _ = caller;
    Json(DescribeResponse {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        abi_version: ABI_VERSION.to_owned(),
        models: app.models_now(),
        tools: app.tools_now(),
        generation: app
            .kernel
            .as_ref()
            .map(|kernel| kernel.generation().0)
            .unwrap_or(0),
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

    let keys = system
        .get(crate::admin::ADMIN_KEYS_KEY)
        .await
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

    let document = crate::configure::document_of(system)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    let tenants = document.tenants.keys().cloned().collect::<Vec<String>>();

    Ok(Json(ConfigResponse {
        document_version: document.document_version,
        revision: document.revision,
        models: document.models.clone(),
        sources: document.sources.clone(),
        tenants: document.tenants.clone(),
        tenant_policies: tenants,
        retention: document.retention.clone(),
        schema_version: system
            .schema_version()
            .await
            .map_err(|error| ApiError::from_error(&error))?,
        root: app.root().map(|root| root.display().to_string()),
        policy_json: document.policy.clone(),
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
/// Replace the whole configuration from one document.
///
/// `If-Match: <revision>` is optional and means "only if the deployment is still at revision N". It is
/// what turns an apply from a last-writer-wins overwrite into a compare-and-swap: without it, an
/// operator holding a document exported an hour ago silently reverts whatever the on-call engineer
/// changed in between, and the history shows two applies and no mistake.
async fn apply_config(
    State(app): State<Arc<App>>,
    headers: axum::http::HeaderMap,
    admin: crate::admin::Admin,
) -> Result<Json<AppliedDeployment>, ApiError> {
    let Some(system) = app.system() else {
        return Err(ApiError::from_error(&nucleos::Error::unavailable(
            "this daemon has no system store",
        )));
    };
    let document: Deployment = serde_json::from_slice(&admin.body).map_err(|error| {
        ApiError::from_error(&nucleos::Error::invalid(format!(
            "the document is not readable: {error}"
        )))
    })?;
    // `If-Match` accepts the weak/quoted form a cache would send as well as a bare number.
    let expected = headers
        .get(axum::http::header::IF_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().trim_matches('"').to_owned())
        .filter(|value| !value.is_empty() && value != "*")
        .map(|value| {
            value.parse::<u64>().map_err(|_| {
                ApiError::from_error(&nucleos::Error::invalid(format!(
                    "If-Match {value:?} is not a revision"
                )))
            })
        })
        .transpose()?;

    let applied = crate::configure::apply(&app, system, document, expected, &admin)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    Ok(Json(AppliedDeployment {
        revision: applied.revision,
        summary: applied.summary,
    }))
}

/// What has been changed and by which key.
///
/// The history existed before this route and nothing read it, which made "who changed the policy" a
/// question for `sqlite3`. Rows carry the signing key now, so the answer is a request.
async fn config_history(
    State(app): State<Arc<App>>,
    Query(query): Query<HistoryQuery>,
    // The history names every change and the key that signed it, so it is an administrative read.
    _admin: crate::admin::Admin,
) -> Result<Json<ChangeListResponse>, ApiError> {
    let Some(system) = app.system() else {
        return Err(ApiError::from_error(&nucleos::Error::unavailable(
            "this daemon has no system store",
        )));
    };
    let limit = if query.limit == 0 { 50 } else { query.limit };
    let rows = system
        .history(limit.min(500))
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    Ok(Json(ChangeListResponse {
        changes: rows
            .into_iter()
            .map(|change| ChangeView {
                at_unix_millis: change.at_unix_millis,
                tenant: change.tenant,
                subject: change.subject,
                key_id: change.key_id,
                key: change.key,
                value: change.value,
            })
            .collect(),
    }))
}

/// Query parameters for the configuration history.
#[derive(Debug, serde::Deserialize)]
struct HistoryQuery {
    /// How many rows, newest first.
    #[serde(default)]
    limit: usize,
}

/// Remove a closed session: its log, its checkpoint, its listing row.
///
/// Only a closed one. Deleting an open session would destroy a conversation somebody may still be
/// using, and "I want it gone" for a running session is `close` first — which the refusal says.
async fn delete_session(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<DeletedResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let session = SessionId::new(id);
    let removed = kernel
        .delete(&session, &caller.0)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    if !removed {
        return Err(ApiError::from_error(&nucleos::Error::not_found(format!(
            "no session {session}"
        ))));
    }
    Ok(Json(DeletedResponse { deleted: true }))
}

async fn metrics(
    State(app): State<Arc<App>>,
    admin: crate::admin::Admin,
) -> Result<Json<MetricsResponse>, ApiError> {
    // Metrics are a deployment-level view: per-route request counts, refusal counts, session counts,
    // and how much retention has deleted. They are not a tenant's view of their own traffic, so they
    // need an administrator's signature rather than a tenant's bearer token.
    let kernel = app.kernel()?.clone();
    match kernel
        .check_config(
            &admin.principal,
            nucleos::adapter::ConfigScope::System,
            "metrics",
        )
        .await
        .map_err(|error| ApiError::from_error(&error))?
    {
        nucleos::adapter::Verdict::Allow => {}
        nucleos::adapter::Verdict::Deny { code, reason } => {
            return Err(ApiError::from_error(&nucleos::Error::new(code, reason)));
        }
        nucleos::adapter::Verdict::AskHuman { question, .. } => {
            return Err(ApiError::from_error(&nucleos::Error::denied(format!(
                "metrics needs a human ({question}), and configuration cannot wait for one"
            ))));
        }
    }
    Ok(Json(app.metrics().snapshot()))
}

/// Reconnect the tool source and publish what it advertises now.
///
/// Forget every generation but the current one.
///
/// Safe to call at any time and safe to ignore: a session pinned to a retired set holds it, so the
/// entry can be dropped without the session noticing. The count that comes back is what makes the claim
/// checkable — "nothing was running" and "everything was running" must not produce the same answer.
async fn retire(
    State(app): State<Arc<App>>,
    // A configuration-shaped action, so it takes the administrative credential and nothing else. It
    // must stay last: it is the extractor that consumes the body the signature covered.
    _admin: crate::admin::Admin,
) -> Result<Json<RetireResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let retired = kernel
        .retire()
        .map_err(|error| ApiError::from_error(&error))?;
    Ok(Json(RetireResponse {
        current: kernel.generation().0,
        retired: retired.into_iter().map(|generation| generation.0).collect(),
    }))
}

/// This is the whole point of generations from the outside: a source is a process somebody else
/// deploys, and reconnecting it must not be a restart. Sessions already running keep the set they
/// are pinned to; sessions that start from now on get this one.
async fn reload(
    State(app): State<Arc<App>>,
    _admin: crate::admin::Admin,
) -> Result<Json<ReloadResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    // Scoped: a read guard held across the reconnect below would make this handler's future
    // non-`Send`, and the whole route would stop compiling.
    let configured = !app.sources().read().await.views().is_empty();
    if !configured {
        return Err(ApiError::from_error(&nucleos::Error::unavailable(
            "no tool source is configured: add one with `nucleos sources add`",
        )));
    }

    let before = app.tools_now();
    let Some(system) = app.system().cloned() else {
        return Err(ApiError::from_error(&nucleos::Error::unavailable(
            "this daemon has no system store",
        )));
    };
    let reconnected = app
        .sources()
        .write()
        .await
        .reconnect(&system, app.seal())
        .await;
    let generation = crate::sources::publish(&app, &kernel).await?;
    app.metrics().reloaded();

    let names = app.tools_now();
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
        sources = reconnected,
        tools = names.len(),
        added = ?added,
        removed = ?removed,
    );

    Ok(Json(ReloadResponse {
        generation: generation.0,
        tools: names,
        added,
        removed,
    }))
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
    // The head, not the whole history: this endpoint is polled, and parsing a long conversation to
    // answer "where is it and what has it spent" is the cost that grew with the session's age.
    let session = load_head(&kernel, &caller, &id).await?;
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

/// One turn, answered at the end or as it happens.
///
/// `Accept: text/event-stream` returns the turn's *events*, one per frame, and nothing else: no
/// second vocabulary, no summary type, no synthesized terminal frame. The last frame is the
/// `message_completed` the kernel already wrote, carrying the assembled message, so a client that
/// ignores every earlier frame ends up with exactly what the non-streaming call returns.
///
/// The split is decided by the log, not by the transport, and the line is `turn_started`. The kernel
/// writes it once the turn is certain to run. A turn that fails before that is an `ApiError` with a
/// status code — even though the kernel may already have written *why* (`budget_exhausted`, say),
/// because that record belongs to the session and not to a stream this request never opened.
async fn inference(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<InferenceRequest>,
) -> Result<Response, ApiError> {
    let kernel = app.kernel()?.clone();
    let mut session = load(&kernel, &caller, &id).await?;

    if !wants_stream(&headers) {
        let outcome = kernel
            .inference(&mut session, &request.messages)
            .await
            .map_err(|error| ApiError::from_error(&error))?;
        return Ok(Json(InferenceResponse {
            message: outcome.message,
            finish: format!("{:?}", outcome.finish).to_lowercase(),
            usage: outcome.usage,
        })
        .into_response());
    }

    let key = session.key();
    // The guard needs its own copy: the pump takes `key` to filter the feed.
    let guard_key = key.clone();
    let principal = session.principal().clone();
    let session_id = session.id().clone();
    // One past the last event, taken before anything can write: this is where this turn's events
    // begin, so a follower that falls behind has a floor to catch up from instead of replaying the
    // whole conversation.
    let start = session.next_seq();
    // Subscribed before the turn starts: the kernel announces as it writes, so a follower that
    // attaches afterwards has already missed the beginning.
    let mut live = kernel.follow();

    // The turn runs in its own task. A client that disconnects must still get a record: a future
    // dropped mid-await writes nothing at all, and the log would show a turn that simply vanished.
    let turn_kernel = Arc::clone(&kernel);
    let messages = request.messages;
    let mut turn =
        tokio::spawn(async move { turn_kernel.inference(&mut session, &messages).await });

    // Before the headers: did the turn start? `turn_started` is the line. Anything earlier is the
    // caller's problem and belongs in a status code.
    //
    // `biased`, and the feed first: when both are ready we want the stream, because frames exist and
    // a client that asked for them should get them. Without this order the choice is random, and the
    // same request would sometimes answer JSON and sometimes SSE.
    // Everything written before the turn was announced. Kept rather than streamed: if the turn never
    // starts, these are part of the refusal, and the caller reads the refusal as a status code.
    let mut buffered: Vec<Event> = Vec::new();
    loop {
        tokio::select! {
            biased;
            frame = live.recv() => match frame {
                Ok(live) if live.key == key => {
                    let announced = matches!(live.event.kind, EventKind::TurnStarted { .. });
                    buffered.push(live.event);
                    if announced {
                        break;
                    }
                }
                Ok(_) => {}
                // A follower that fell behind catches up from the store, which is the truth.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return Err(ApiError::from_error(&nucleos::Error::internal(
                        "the event feed closed while the turn was starting",
                    )));
                }
            },
            outcome = &mut turn => {
                // The turn ended without writing anything: this is a refusal, not a stream.
                return match outcome {
                    Ok(Ok(outcome)) => Ok(Json(InferenceResponse {
                        message: outcome.message,
                        finish: format!("{:?}", outcome.finish).to_lowercase(),
                        usage: outcome.usage,
                    })
                    .into_response()),
                    Ok(Err(error)) => Err(ApiError::from_error(&error)),
                    Err(join) => Err(ApiError::from_error(&nucleos::Error::internal(
                        format!("the turn ended unexpectedly: {join}"),
                    ))),
                };
            }
        }
    }

    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<SseEvent, Infallible>>(64);

    let pump_kernel = Arc::clone(&kernel);
    let pump_session = session_id.clone();
    let pump_principal = principal.clone();
    tokio::spawn(async move {
        let mut last = buffered
            .last()
            .map_or_else(|| start.saturating_sub(1), |event| event.seq);
        for event in &buffered {
            if sender.send(Ok(sse_frame(event))).await.is_err() {
                return;
            }
        }
        loop {
            tokio::select! {
                frame = live.recv() => match frame {
                    Ok(live) if live.key == key => {
                        last = live.event.seq;
                        if sender.send(Ok(sse_frame(&live.event))).await.is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Behind, not broken. Everything between `last + 1` and now is in the store,
                        // which is the record; the broadcast was only ever the fast path.
                        if !catch_up(
                            &pump_kernel,
                            &pump_principal,
                            &pump_session,
                            &mut last,
                            &sender,
                        )
                        .await
                        {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
                outcome = &mut turn => {
                    // The turn is over. Whatever it wrote is already in the broadcast queue, so drain
                    // it before closing — otherwise the last frames race the terminal one, and the
                    // terminal one is the whole promise this endpoint makes.
                    //
                    // `Lagged` here is not the end of the drain: a client more than a channel deep
                    // behind would otherwise lose exactly the frame it was waiting for. The store has
                    // it, and the store is the record.
                    loop {
                        match live.try_recv() {
                            Ok(live) if live.key == key => {
                                if sender.send(Ok(sse_frame(&live.event))).await.is_err() {
                                    return;
                                }
                            }
                            Ok(_) => {}
                            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
                            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                                let mut drained = last;
                                if !catch_up(
                                    &pump_kernel,
                                    &pump_principal,
                                    &pump_session,
                                    &mut drained,
                                    &sender,
                                )
                                .await
                                {
                                    return;
                                }
                            }
                        }
                    }
                    match outcome {
                        Ok(Ok(_)) | Ok(Err(_)) => {}
                        Err(join) => {
                            tracing::error!(event = "turn_task_failed", detail = %join);
                        }
                    }
                    return;
                }
            }
        }
    });

    // The turn is cancelled by *this body being dropped*, not by a second task watching the channel.
    // A task holding a sender of its own would keep the channel open after the turn ended, and the
    // response would never finish — which is the deadlock this endpoint would have shipped with.
    Ok(Sse::new(CancelOnDrop {
        inner: ReceiverStream::new(receiver),
        kernel,
        session: guard_key,
        armed: true,
    })
    // A comment frame while the model is thinking: without it a silent provider is indistinguishable
    // from a connection that died, which is the ambiguity this endpoint exists to remove.
    .keep_alive(KeepAlive::default())
    .into_response())
}

/// A body that cancels the turn it belongs to when it is dropped.
///
/// Dropping the body is what a client hanging up looks like from here, and it is the only signal that
/// arrives promptly: a task waiting on a channel cannot tell a slow reader from a gone one.
///
/// Disarmed when the stream ends on its own, so the common case — the turn finished and the client
/// read it — does not send a cancel for a turn that no longer exists, which would be a confusing
/// `turn_cancelled_by_disconnect` in the log for a client that behaved perfectly.
struct CancelOnDrop {
    inner: ReceiverStream<Result<SseEvent, Infallible>>,
    kernel: Arc<Kernel>,
    /// The log's key, not the id: cancelling by id alone reaches across tenants that chose the same
    /// name, and the guard fires on a client hanging up — nobody is watching what it hits.
    session: nucleos::adapter::SessionKey,
    armed: bool,
}

impl tokio_stream::Stream for CancelOnDrop {
    type Item = Result<SseEvent, Infallible>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = &mut *self;
        let polled = std::pin::Pin::new(&mut this.inner).poll_next(context);
        if matches!(polled, std::task::Poll::Ready(None)) {
            this.armed = false;
        }
        polled
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed && self.kernel.cancel(&self.session) {
            tracing::info!(
                event = "turn_cancelled_by_disconnect",
                session = %self.session.id,
                tenant = %self.session.tenant,
            );
        }
    }
}

/// Send everything the store holds from `last + 1`, and move `last` along.
///
/// The broadcast is the fast path and the store is the record: this is what a follower calls when it
/// has fallen behind, in the stream and in the terminal drain alike. `false` means the client is gone.
async fn catch_up(
    kernel: &Arc<Kernel>,
    principal: &Principal,
    session: &SessionId,
    last: &mut u64,
    sender: &tokio::sync::mpsc::Sender<Result<SseEvent, Infallible>>,
) -> bool {
    let Ok(events) = kernel
        .events(principal, session, last.saturating_add(1))
        .await
    else {
        return false;
    };
    for event in events {
        *last = event.seq;
        if sender.send(Ok(sse_frame(&event))).await.is_err() {
            return false;
        }
    }
    true
}

/// Whether the caller asked for the turn as it happens.
fn wants_stream(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/event-stream"))
}

/// One event, as a frame.
///
/// The single place frames are made. `id` is the log's sequence, so a client that reconnects can
/// continue on `GET /v1/sessions/{id}/events` from where it stopped — the same cursor for both
/// endpoints, because there is one stream of events and these are two ways to read it.
fn sse_frame(event: &Event) -> SseEvent {
    SseEvent::default()
        .event(event.kind.type_name())
        .id(event.seq.to_string())
        .data(serde_json::to_string(event).unwrap_or_else(|_| "{}".to_owned()))
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
        credit_tokens: Some(0),
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

/// The sessions this caller owns.
///
/// The store could always answer this — `Session::` listings were only exposed for processes, so an
/// application that opened agent sessions had no way to enumerate what it had created. A listing is a
/// page of summaries, not of conversations: a hundred sessions must not load a hundred histories.
/// Cancel the turn a session is running.
///
/// Not a close: the session stays open, what the attempt produced stays in the log, and the spend that
/// was reported stays debited. `cancelled: false` is not an error — a caller racing the end of a turn is
/// a normal race, and answering `404` would tell it the session is gone.
async fn cancel_turn(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<CancelResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let session = load(&kernel, &caller, &id).await?;
    let cancelled = kernel.cancel(&session.key());
    tracing::info!(event = "turn_cancelled", session = %id, cancelled);
    Ok(Json(CancelResponse { cancelled }))
}

async fn list_sessions(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(query): Query<SessionsQuery>,
) -> Result<Json<SessionListResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let store = app
        .store
        .clone()
        .ok_or_else(|| ApiError::from_error(&nucleos::Error::unavailable("no store")))?;

    // An unknown kind is a client bug, not an empty page: answering "nothing" to a misspelled filter is
    // how a caller concludes the feature is broken.
    let kind = match query.kind.as_deref() {
        None | Some("") => None,
        Some("agent") => Some(nucleos::contract::SessionKind::Agent),
        Some("process") => Some(nucleos::contract::SessionKind::Process),
        Some(other) => {
            return Err(ApiError::from_error(&nucleos::Error::invalid(format!(
                "unknown kind {other:?}; expected agent or process"
            ))));
        }
    };

    // Bounded, like the process listing: an unbounded `?limit=` makes one request read a whole
    // tenant's sessions one snapshot at a time.
    let limit = if query.limit == 0 {
        20
    } else {
        query.limit.min(500)
    };
    let page = store
        .sessions(&nucleos::adapter::SessionQuery {
            tenant: caller.0.tenant.clone(),
            // Filtered by the store, not by this loop. Narrowing a page after the store has chosen it
            // can return a short page *and* a cursor — the client is told there is more and that it
            // has seen everything, in the same response.
            subject: Some(caller.0.subject.clone()),
            kind,
            limit: limit as usize,
            after: query
                .page_token
                .clone()
                .map(nucleos::contract::SessionId::new),
        })
        .await
        .map_err(|error| ApiError::from_error(&error))?;

    let mut sessions = Vec::new();
    for id in page.sessions {
        let key = nucleos::adapter::SessionKey {
            tenant: caller.0.tenant.clone(),
            id: id.clone(),
        };
        // The head, not the whole session: a summary needs kind, state, stop and cursor, and reading
        // the full snapshot would reassemble every message and every attempt to answer those four.
        let Some(snapshot) = store
            .snapshot_head(&key)
            .await
            .map_err(|error| ApiError::from_error(&error))?
        else {
            // A session with events but no checkpoint yet: it exists, and nothing about it is known.
            continue;
        };
        sessions.push(SessionSummary {
            session: id.as_str().to_owned(),
            kind: snapshot.kind,
            state: snapshot.state,
            stop: snapshot.stop,
            next_seq: snapshot.seq + 1,
        });
    }
    let _ = kernel;

    Ok(Json(SessionListResponse {
        sessions,
        next_page_token: page.next.map(|next| next.as_str().to_owned()),
    }))
}

/// Query parameters for the session listing.
#[derive(Debug, serde::Deserialize)]
struct SessionsQuery {
    /// `agent`, `process`, or absent for both.
    #[serde(default)]
    kind: Option<String>,
    /// Page size. Zero asks for the default.
    #[serde(default)]
    limit: u32,
    /// Continue after this id.
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
        .processes(
            &caller.0.tenant,
            &nucleos::contract::ProcessQuery {
                phase,
                limit: query.limit,
                after: query.page_token.map(nucleos::contract::SessionId::new),
            },
        )
        .await
        .map_err(|error| ApiError::from_error(&error))?;

    Ok(Json(ProcessListResponse {
        processes: page
            .processes
            .into_iter()
            .map(|summary| ProcessInfo {
                session: summary.session.as_str().to_owned(),
                argv: summary.argv,
                cwd: summary.cwd,
                status: summary.status,
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

/// Grant credit, and revive a session that stopped on a budget.
async fn credit(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Json(request): Json<CreditRequest>,
) -> Result<Json<CreditResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let mut session = load(&kernel, &caller, &id).await?;
    kernel
        .credit(&mut session, request.amount)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    Ok(Json(CreditResponse {
        session: session.id().as_str().to_owned(),
        state: if session.is_closed() {
            nucleos::contract::SessionState::Closed
        } else {
            nucleos::contract::SessionState::Open
        },
        stop: session.stop_reason(),
        credit_tokens: session.credit(),
        next_seq: session.next_seq(),
    }))
}

/// Answer the question a session is parked on.
///
/// One route for a tool call and for a command: both are questions the kernel asked, and both are
/// answered the same way. Nothing is driven afterwards — whether another turn follows is the caller's
/// decision.
async fn answer(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Json(request): Json<AnswerRequest>,
) -> Result<Json<StartResponse>, ApiError> {
    let kernel = app.kernel()?.clone();
    let mut session = load(&kernel, &caller, &id).await?;
    kernel
        .answer(
            &mut session,
            nucleos::kernel::Approval {
                pending_id: request.pending_id,
                option_id: request.option_id,
            },
        )
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    Ok(Json(StartResponse {
        session: session.id().as_str().to_owned(),
        state: if session.is_closed() {
            SessionState::Closed
        } else {
            SessionState::Open
        },
        stop: session.stop_reason(),
        next_seq: session.next_seq(),
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

/// How many events one page of a replay holds.
///
/// Small enough that the first frame arrives immediately, large enough that a long history is a few
/// round trips to the store rather than hundreds.
const PAGE: usize = 256;

/// The reader's place in a replay: the kernel, whose log, and where to continue.
struct Pages {
    kernel: Arc<Kernel>,
    principal: Principal,
    id: SessionId,
    next: u64,
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

    // Loaded first, for the ownership check. `events` is scoped by tenant, which is the wrong
    // question here: a session id is caller-chosen and short, so within one tenant every subject can
    // guess every other subject's, and the log is the whole conversation including tool output. This
    // is the one line between "I can read my session's events" and "I can read yours".
    let _ = load_head(&kernel, &caller, &id).await?;

    // Paged and lazy. The old shape read the whole log into a `Vec` before sending the first frame,
    // so both the latency to first byte and the memory were functions of the session's age. A pump
    // with a bounded channel is the same shape the inference stream uses.
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<SseEvent, Infallible>>(PAGE);
    let pages = Pages {
        kernel,
        principal: caller.0.clone(),
        id: SessionId::new(id),
        next: from_seq,
    };
    tokio::spawn(async move {
        let Pages {
            kernel,
            principal,
            id,
            mut next,
        } = pages;
        loop {
            let Ok(page) = kernel.events_page(&principal, &id, next, PAGE).await else {
                return;
            };
            if page.is_empty() {
                return;
            }
            let short = page.len() < PAGE;
            for event in page {
                next = event.seq.saturating_add(1);
                if sender.send(Ok(sse_frame(&event))).await.is_err() {
                    return;
                }
            }
            // A short page means the store had nothing more; a full one means there may be another.
            if short {
                return;
            }
        }
    });
    Ok(Sse::new(ReceiverStream::new(receiver)))
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

/// The same session, without its conversation.
async fn load_head(
    kernel: &Kernel,
    caller: &Caller,
    id: &str,
) -> Result<nucleos::kernel::Session, ApiError> {
    kernel
        .load_head(&caller.0, &SessionId::new(id))
        .await
        .map_err(|error| ApiError::from_error(&error))
}

/// A failure, as the wire carries it.
pub struct ApiError {
    status: StatusCode,
    body: ErrorBody,
}

impl ApiError {
    /// The same failure, in the kernel's vocabulary.
    ///
    /// For helpers outside this module that speak `nucleos::Error` and still have to call something
    /// that answers a status code.
    pub(crate) fn into_error(self) -> nucleos::Error {
        nucleos::Error::new(self.body.code, self.body.reason)
    }

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
            // 409 Conflict, not 500: the turn was aborted by the caller's own decision, and the
            // session is still usable. Nothing went wrong, and the code should not suggest it did.
            ErrorCode::Cancelled => StatusCode::CONFLICT,
            ErrorCode::Conflict => StatusCode::CONFLICT,
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
        let layout = crate::layout::Layout::resolve(
            Some(root.clone()),
            None,
            Some(root.with_extension("seal.key")),
        );

        // A store that exists and is empty: `init` has not run.
        SystemStore::open(layout.system_store()).unwrap();
        let Err(error) = crate::http::App::load(&layout).await else {
            panic!("a store without a policy must not start");
        };
        assert!(error.reason().contains("nucleos init"), "{error:?}");

        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod correlation_tests {
    use super::session_in;

    /// The id a log line carries is the id in the path, and nothing else.
    #[test]
    fn the_session_is_the_segment_after_the_prefix() {
        assert_eq!(
            session_in("/v1/sessions/s1/inference").as_deref(),
            Some("s1")
        );
        assert_eq!(session_in("/v1/sessions/s1").as_deref(), Some("s1"));
        assert_eq!(session_in("/v1/sessions/s1/cancel").as_deref(), Some("s1"));
        // Not a session route, and not a prefix that merely looks like one.
        assert_eq!(session_in("/v1/sessions"), None);
        assert_eq!(session_in("/v1/sessions/"), None);
        assert_eq!(session_in("/v1/processes"), None);
        assert_eq!(session_in("/v1/sessionsx/s1"), None);
    }
}
