//! A tool source: one MCP server, and the tools it advertises.
//!
//! A source is declared, never inlined. The kernel connects, asks what the server offers, and holds
//! the answers as ordinary [`Tool`]s — which is why a tool from another process needs no special
//! case in the engine: policy gates it, the ledger counts it, and the event log records it like any
//! other.
//!
//! The protocol itself is [`rmcp`]: version negotiation, cancellation, progress and the rest of the
//! specification belong to it, not here. What this module owns is the shape the kernel sees.

use crate::content;
use async_trait::async_trait;
use nucleos::adapter::{Tool, ToolCtx, ToolOutput};
use nucleos::contract::{Isolation, ToolSpec};
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

        let mut tools = Vec::new();
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
                tools.push(McpTool {
                    server: Arc::clone(&server),
                    spec: spec_of(declaration),
                    timeout: self.call_timeout,
                });
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

        if tools.is_empty() {
            return Err(Error::unavailable(format!(
                "source {:?} advertises no tools",
                self.name
            )));
        }
        Ok(tools)
    }
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
    }
}

/// One tool, executed by the server that advertises it.
pub struct McpTool {
    server: Arc<Server>,
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

    /// A tool served over stdio runs in another process by construction — which is the isolation
    /// story this whole design prefers: a crashing tool is an unhealthy source, not a crashed
    /// kernel.
    fn isolation(&self) -> Isolation {
        Isolation::Process
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

        let call = self.server.call_tool(
            CallToolRequestParams::new(self.spec.name.clone()).with_arguments(arguments),
        );

        let result = match self.timeout_for(cx) {
            // The transport cancels the request in flight when the future is dropped, which is what
            // makes this a real timeout rather than a client that stops listening.
            Some(timeout) => match tokio::time::timeout(timeout, call).await {
                Ok(result) => result,
                Err(_) => {
                    return ToolOutput::failed(format!(
                        "the tool did not answer within {}ms",
                        timeout.as_millis()
                    ));
                }
            },
            None => call.await,
        };

        match result {
            Ok(result) => {
                let (content, is_error) = content::decode(&result);
                ToolOutput {
                    content,
                    is_error,
                    usage: None,
                }
            }
            // A protocol error is a failure of the *source*, and the model is told in the only
            // language it reads: content.
            Err(error) => ToolOutput::failed(error.to_string()),
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
