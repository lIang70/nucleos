//! JSON-RPC 2.0 over a child process, one object per line.
//!
//! The transport is the whole of the protocol here: a request is a line, a response is a line, and
//! anything without an `id` is a notification the kernel has no use for.

use nucleos::error::{Error, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};

/// A connection to one MCP server.
///
/// One reader task dispatches responses to whoever is waiting for that id; the write side is a
/// cloneable handle. Killing the child closes the connection, which is the only shutdown this
/// protocol has.
pub struct Client {
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    pending: Arc<Mutex<BTreeMap<u64, oneshot::Sender<Value>>>>,
    next_id: AtomicU64,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

impl Client {
    /// Start a server and complete the handshake.
    ///
    /// The handshake is part of `spawn` rather than a separate step because a connection that has
    /// not been initialised is not usable: `tools/list` before it is a protocol error, and making
    /// that the caller's problem invites a race.
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let mut child = Command::new(command)
            .args(args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A server must not outlive the kernel's interest in it.
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| Error::unavailable(format!("cannot start {command:?}: {error}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::internal("no stdin on the child"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::internal("no stdout on the child"))?;

        let pending: Arc<Mutex<BTreeMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(BTreeMap::new()));

        tokio::spawn(dispatch(BufReader::new(stdout), Arc::clone(&pending)));

        let client = Self {
            child: Arc::new(Mutex::new(child)),
            stdin: Arc::new(Mutex::new(stdin)),
            pending,
            next_id: AtomicU64::new(0),
        };

        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "nucleos", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await?;
        // The protocol requires this before any other request, and says nothing if it is missing.
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(client)
    }

    /// Send a request, and wait for its response.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.write(&line.to_string()).await?;

        let response = rx
            .await
            .map_err(|_| Error::unavailable(format!("{method}: the server went away")))?;

        if let Some(error) = response.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the server refused the call");
            let code = error.get("code").and_then(Value::as_i64);
            let error = Error::unavailable(format!("{method}: {message}"));
            return Err(match code {
                Some(code) => error.detail("jsonrpc_code", code.to_string()),
                None => error,
            });
        }

        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Send a notification. No id, so no answer.
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let line = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.write(&line.to_string()).await
    }

    /// Stop the server.
    pub async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        let _ = child.start_kill();
        let _ = child.wait().await;
    }

    async fn write(&self, line: &str) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        // One write for the line and its delimiter: two would let a reader see half a message.
        let mut message = line.to_owned();
        message.push('\n');
        stdin
            .write_all(message.as_bytes())
            .await
            .map_err(|error| Error::unavailable(format!("cannot write to the server: {error}")))?;
        stdin
            .flush()
            .await
            .map_err(|error| Error::unavailable(format!("cannot flush to the server: {error}")))
    }
}

/// Read lines and hand each response to whoever is waiting for its id.
async fn dispatch(
    reader: BufReader<tokio::process::ChildStdout>,
    pending: Arc<Mutex<BTreeMap<u64, oneshot::Sender<Value>>>>,
) {
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(trimmed) else {
            // A server that writes a log line to stdout is broken, but it is not our crash.
            continue;
        };
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            // A notification: there is nothing to answer, and nothing here wants one.
            continue;
        };
        if let Some(sender) = pending.lock().await.remove(&id) {
            // The receiver may have gone away with its caller; that is not an error worth logging.
            let _ = sender.send(message);
        }
    }
    // The reader stopped: every waiter is now waiting for something that will never come.
    pending.lock().await.clear();
}
