//! A tool source: one MCP server, and the tools it advertises.
//!
//! A source is declared, never inlined. The kernel connects, asks what the server offers, and holds
//! the answers as ordinary [`Tool`]s — which is why a tool from another process needs no special
//! case in the engine: policy gates it, the ledger counts it, and the event log records it like any
//! other.
//!
//! A source is a child process, so it dies: it crashes, someone deploys over it, the host reclaims
//! memory. A tool whose server is gone must not be a tool that fails forever, so the connection
//! sits behind a shared cell that a call can rebuild. What a rebuild may **not** do is change the
//! answer: the tools are pinned, and a server that comes back advertising a different set is a new
//! source, not a reconnected one — a session was promised these declarations.
//!
//! The protocol itself is [`rmcp`]: version negotiation, cancellation, progress and the rest of the
//! specification belong to it, not here. What this module owns is the shape the kernel sees.

use crate::content;
use async_trait::async_trait;
use nucleos::adapter::{Tool, ToolCtx, ToolOutput};
use nucleos::contract::ToolSpec;
use nucleos::error::{Error, Result};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, Tool as McpDeclaration};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One connection to a server, shared by every tool it advertises.
type Server = RunningService<RoleClient, ()>;

/// How long to wait after the first failed reconnect, doubled on each further failure.
const BACKOFF_BASE: Duration = Duration::from_millis(250);
/// The longest a source waits between attempts.
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// The connection, and the state of trying to have one.
struct Link {
    /// `None` once the child is known to be gone.
    server: Option<Arc<Server>>,
    /// Consecutive failed attempts, which is what the backoff is computed from.
    failures: u32,
    /// Do not try again before this. A source whose binary was moved would otherwise be spawned
    /// once per call for as long as the session lasts.
    retry_after: Option<tokio::time::Instant>,
}

/// How to reach one MCP server.
#[derive(Debug, Clone)]
pub struct McpSource {
    /// A name for logs and for `Describe`.
    pub name: String,
    /// The command that starts it.
    pub command: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Extra environment.
    pub env: BTreeMap<String, String>,
    /// A ceiling on one call, when the session has none of its own.
    pub call_timeout: Option<Duration>,
}

impl McpSource {
    /// A stdio server, started with a command.
    #[must_use]
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            call_timeout: Some(Duration::from_secs(60)),
        }
    }

    /// Arguments for the command.
    #[must_use]
    pub fn with_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// A ceiling on one call, when the session has none of its own.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = Some(timeout);
        self
    }

    /// Start the server and ask what it offers.
    pub async fn connect(&self) -> Result<Vec<McpTool>> {
        let (server, specs) = self.spawn().await?;
        let hub = Arc::new(Hub {
            source: self.clone(),
            pinned: specs.clone(),
            link: tokio::sync::Mutex::new(Link {
                server: Some(server),
                failures: 0,
                retry_after: None,
            }),
        });

        Ok(specs
            .into_iter()
            .map(|spec| McpTool {
                hub: Arc::clone(&hub),
                spec,
                timeout: self.call_timeout,
            })
            .collect())
    }

    /// Start the child, serve the protocol, and ask what it offers.
    ///
    /// Everything that can go wrong with a source happens here, so there is one place that knows a
    /// source is down.
    async fn spawn(&self) -> Result<(Arc<Server>, Vec<ToolSpec>)> {
        let mut command = tokio::process::Command::new(&self.command);
        command.args(&self.args).envs(&self.env);

        // The transport owns the child: cancelling a call and killing the server are its job, and a
        // second owner for the pipe is how a client and a transport start disagreeing.
        let transport = TokioChildProcess::new(command).map_err(|error| {
            Error::unavailable(format!(
                "cannot start {:?} for source {:?}: {error}",
                self.command, self.name
            ))
        })?;
        let server: Arc<Server> = Arc::new(().serve(transport).await.map_err(|error| {
            Error::unavailable(format!(
                "source {:?} refused to initialise: {error}",
                self.name
            ))
        })?);

        let mut specs = Vec::new();
        let mut cursor = None;
        // `tools/list` is paginated, and a server with more than one page is the normal case for
        // anything generated.
        loop {
            let page = server.list_tools(cursor.clone()).await.map_err(|error| {
                Error::unavailable(format!(
                    "source {:?}: cannot list tools: {error}",
                    self.name
                ))
            })?;

            for declaration in &page.tools {
                specs.push(spec_of(declaration));
            }

            match page.next_cursor {
                Some(next) if !next.is_empty() => {
                    cursor = Some(
                        rmcp::model::PaginatedRequestParams::default().with_cursor(Some(next)),
                    );
                }
                _ => break,
            }
        }

        if specs.is_empty() {
            return Err(Error::unavailable(format!(
                "source {:?} advertises no tools",
                self.name
            )));
        }
        tracing::info!(
            event = "source_connected",
            source = %self.name,
            tools = specs.len(),
        );
        Ok((server, specs))
    }
}

/// One source's live connection, shared by every tool it advertises.
///
/// The cell exists because the tools outlive the process: the kernel registered them in a
/// generation, and replacing the objects whenever a server restarted would be a new generation on
/// every crash.
struct Hub {
    source: McpSource,
    /// The declarations the session was promised. A reconnected server has to agree.
    pinned: Vec<ToolSpec>,
    link: tokio::sync::Mutex<Link>,
}

impl Hub {
    /// The current connection, reconnecting if it is known to be gone.
    ///
    /// Serialised through the mutex on purpose: two calls that arrive while the source is down must
    /// produce one child, not one child each.
    async fn server(&self) -> Result<Arc<Server>> {
        let mut link = self.link.lock().await;
        if let Some(server) = &link.server {
            return Ok(Arc::clone(server));
        }
        if let Some(retry_after) = link.retry_after
            && tokio::time::Instant::now() < retry_after
        {
            return Err(Error::unavailable(format!(
                "source {:?} is down; next attempt in {}ms",
                self.source.name,
                retry_after
                    .saturating_duration_since(tokio::time::Instant::now())
                    .as_millis()
            )));
        }

        match self.source.spawn().await {
            Ok((server, specs)) => {
                // A server that comes back with different tools is not the source the session was
                // pinned to: adopting it would make a generation describe declarations that no
                // longer exist, and the session would call names it was never shown.
                if specs != self.pinned {
                    tracing::warn!(
                        event = "source_set_changed",
                        source = %self.source.name,
                        before = self.pinned.len(),
                        after = specs.len(),
                    );
                    link.failures = link.failures.saturating_add(1);
                    link.retry_after = Some(backoff(link.failures));
                    return Err(Error::unavailable(format!(
                        "source {:?} came back with a different tool set ({} before, {} now); \
                         that is a new generation, not a reconnect",
                        self.source.name,
                        self.pinned.len(),
                        specs.len()
                    )));
                }
                tracing::info!(
                    event = "source_reconnected",
                    source = %self.source.name,
                    failures = link.failures,
                );
                link.server = Some(Arc::clone(&server));
                link.failures = 0;
                link.retry_after = None;
                Ok(server)
            }
            Err(error) => {
                link.failures = link.failures.saturating_add(1);
                link.retry_after = Some(backoff(link.failures));
                tracing::warn!(
                    event = "source_restart_failed",
                    source = %self.source.name,
                    failures = link.failures,
                    detail = %error.reason(),
                );
                Err(error)
            }
        }
    }

    /// Stop using this connection. The next call reconnects.
    async fn mark_dead(&self) {
        let mut link = self.link.lock().await;
        link.server = None;
    }
}

/// How long to wait after `failures` consecutive failed attempts.
fn backoff(failures: u32) -> tokio::time::Instant {
    // `saturating` shifts would let a long outage overflow into an instant retry, which is the one
    // outcome backoff exists to prevent.
    let shift = failures.saturating_sub(1).min(16);
    let delay = BACKOFF_BASE.saturating_mul(1u32 << shift).min(BACKOFF_CAP);
    tokio::time::Instant::now() + delay
}

/// Whether a failed call means the connection is gone.
///
/// A tool that answered with an error is a tool that answered: the model has to read that, and
/// reconnecting would be treating a refusal as a crash. Only a transport-level failure is evidence
/// that there is no longer a server.
fn is_fatal(error: &rmcp::service::ServiceError) -> bool {
    matches!(
        error,
        rmcp::service::ServiceError::TransportSend(_)
            | rmcp::service::ServiceError::TransportClosed
    )
}

/// The kernel's view of one declaration.
fn spec_of(declaration: &McpDeclaration) -> ToolSpec {
    ToolSpec {
        name: declaration.name.to_string(),
        description: declaration
            .description
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default(),
        // Kept as text: the kernel compares schemas structurally and never interprets them.
        input_schema_json: serde_json::Value::Object((*declaration.input_schema).clone())
            .to_string(),
        isolation: Some(nucleos::contract::Isolation::InProcess),
    }
}

/// One tool, executed by the server that advertises it.
pub struct McpTool {
    /// The connection, which may be replaced under it. This is why a tool survives a crash.
    hub: Arc<Hub>,
    spec: ToolSpec,
    timeout: Option<Duration>,
}

impl std::fmt::Debug for McpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTool")
            .field("name", &self.spec.name)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn invoke(&self, args_json: &str, cx: &ToolCtx<'_>) -> ToolOutput {
        let arguments = match serde_json::from_str::<serde_json::Value>(args_json) {
            Ok(serde_json::Value::Object(map)) => map,
            // The kernel validates the JSON before calling, so this is a caller that was not the
            // kernel — and the model still deserves to be told.
            Ok(_) => return ToolOutput::failed("arguments must be a JSON object"),
            Err(error) => {
                return ToolOutput::failed(format!("arguments were not valid JSON: {error}"));
            }
        };

        // Getting the connection is step zero: a source that is down is reported as a source that
        // is down, not as a tool that failed.
        let server = match self.hub.server().await {
            Ok(server) => server,
            Err(error) => return ToolOutput::failed(error.to_string()),
        };

        // Progress from the server is not forwarded. `rmcp` assigns the progress token itself, per
        // request, and overwrites one a caller puts in `_meta`; reading the assigned token back needs
        // `send_cancellable_request`, whose request value the library does not let a caller build.
        // So a server's `notifications/progress` never reaches the sink — the kernel's own events are
        // wired, the transport is not. See docs/pitfalls.md.
        let call = server.call_tool(
            CallToolRequestParams::new(self.spec.name.clone()).with_arguments(arguments),
        );

        let forwarding = call;

        let result = match self.timeout_for(cx) {
            // The transport cancels the request in flight when the future is dropped, which is what
            // makes this a real timeout rather than a client that stops listening.
            Some(timeout) => match tokio::time::timeout(timeout, forwarding).await {
                Ok(result) => result,
                Err(_) => {
                    return ToolOutput::failed(format!(
                        "the tool did not answer within {}ms",
                        timeout.as_millis()
                    ));
                }
            },
            None => forwarding.await,
        };

        match result {
            Ok(result) => {
                let (content, is_error) = content::decode(&result);
                ToolOutput {
                    content,
                    is_error,
                    usage: None,
                    expose: Vec::new(),
                }
            }
            Err(error) => {
                if is_fatal(&error) {
                    // The child is gone. Mark it and try to restore the connection now, so the
                    // *next* call is clean — but do not retry this one: the server may have run the
                    // call before the pipe broke, and a tool call is not assumed idempotent.
                    self.hub.mark_dead().await;
                    return ToolOutput::failed(match self.hub.server().await {
                        Ok(_) => format!(
                            "the tool source was lost: {error}; the connection was restored, so                              this call can be retried"
                        ),
                        // Why a restart failed is what tells the model whether retrying is
                        // pointless, so it is not summarized away.
                        Err(restart) => format!(
                            "the tool source was lost: {error}; restarting it failed: {restart}"
                        ),
                    });
                }
                // A protocol error is a failure of the *source*, and the model is told in the only
                // language it reads: content.
                ToolOutput::failed(error.to_string())
            }
        }
    }
}

impl McpTool {
    /// The session's deadline if it has one, the source's ceiling otherwise.
    fn timeout_for(&self, cx: &ToolCtx<'_>) -> Option<Duration> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64);
        if cx.deadline_unix_millis > now && cx.deadline_unix_millis != u64::MAX {
            return Some(Duration::from_millis(cx.deadline_unix_millis - now));
        }
        self.timeout
    }
}
