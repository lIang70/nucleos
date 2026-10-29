//! A tool source: one MCP server, and the tools it advertises.
//!
//! A source is declared, never inlined. The kernel connects, asks what the server offers, and holds
//! the answers as ordinary [`Tool`]s — which is why a tool from another process needs no special
//! case in the engine: policy gates it, the ledger counts it, and the event log records it like any
//! other.

use crate::client::Client;
use crate::content;
use async_trait::async_trait;
use nucleos::adapter::{Tool, ToolCtx, ToolOutput};
use nucleos::contract::{Isolation, ToolSpec};
use nucleos::error::{Error, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

    /// Connect, hand it the environment, and ask what it offers.
    pub async fn connect(&self) -> Result<Vec<McpTool>> {
        let client = Arc::new(Client::spawn(&self.command, &self.args, &self.env).await?);
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;

        // `tools/list` is paginated, and a server that has more than one page is the normal case for
        // anything generated.
        loop {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let page = client.request("tools/list", params).await?;

            for tool in page
                .get("tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(name) = tool.get("name").and_then(Value::as_str) else {
                    continue;
                };
                tools.push(McpTool {
                    client: Arc::clone(&client),
                    spec: ToolSpec {
                        name: name.to_owned(),
                        description: tool
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        // Kept as text: the kernel compares schemas structurally and never
                        // interprets them.
                        input_schema_json: tool
                            .get("inputSchema")
                            .map(|schema| schema.to_string())
                            .unwrap_or_else(|| "{}".to_owned()),
                    },
                    timeout: self.call_timeout,
                });
            }

            match page.get("nextCursor").and_then(Value::as_str) {
                Some(next) => cursor = Some(next.to_owned()),
                None => break,
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

/// One tool, executed by the server that advertises it.
pub struct McpTool {
    client: Arc<Client>,
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
        let arguments = match serde_json::from_str::<Value>(args_json) {
            Ok(Value::Object(map)) => Value::Object(map),
            // The kernel validates the JSON before calling, so this is a caller that was not the
            // kernel — and the model still deserves to be told.
            Ok(_) => return ToolOutput::failed("arguments must be a JSON object"),
            Err(error) => {
                return ToolOutput::failed(format!("arguments were not valid JSON: {error}"));
            }
        };

        let timeout = self.timeout_for(cx);
        let request = self.client.request(
            "tools/call",
            json!({ "name": self.spec.name, "arguments": arguments }),
        );

        let result = match timeout {
            Some(timeout) => match tokio::time::timeout(timeout, request).await {
                Ok(result) => result,
                Err(_) => {
                    return ToolOutput::failed(format!(
                        "the tool did not answer within {}ms",
                        timeout.as_millis()
                    ));
                }
            },
            None => request.await,
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
            Err(error) => ToolOutput::failed(error.reason().to_owned()),
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
