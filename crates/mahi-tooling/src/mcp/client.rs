//! [`McpClient`]: stdio transport client for one MCP server.
//!
//! Spawns the configured command with piped stdin/stdout, exchanges
//! newline-delimited JSON-RPC messages, and routes responses back to waiting
//! callers by request id. [`McpClient::connect`] performs the MCP handshake
//! (`initialize` → `notifications/initialized` → `tools/list`) and caches the
//! advertised tool list.

use crate::mcp::config::McpServerConfig;
use crate::mcp::protocol::{
    self, CallToolResult, ListToolsResult, Notification, Request, Response, ToolInfo,
};
use crate::mcp::McpError;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

/// Default per-request timeout (handshake, tools/list, ...).
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// `tools/call` gets longer: remote tools may do real work.
const CALL_TOOL_TIMEOUT: Duration = Duration::from_secs(120);

/// In-flight requests awaiting a response, keyed by JSON-RPC id.
type PendingMap = Arc<StdMutex<HashMap<u64, oneshot::Sender<Response>>>>;

/// A connected MCP server reachable over stdio.
///
/// Created with [`McpClient::connect`]; typically wrapped in an [`Arc`] and
/// handed to [`crate::ToolRegistry::attach_mcp_server`]. The child process is
/// spawned with `kill_on_drop`, so dropping the client tears the server down.
pub struct McpClient {
    server_name: String,
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    pending: PendingMap,
    next_id: AtomicU64,
    tools: RwLock<Vec<ToolInfo>>,
    reader: JoinHandle<()>,
}

impl McpClient {
    /// Spawn the server process described by `config` and run the MCP
    /// handshake: `initialize`, the `notifications/initialized` notification,
    /// then `tools/list` (cached, see [`Self::list_tools`]).
    pub async fn connect(config: &McpServerConfig) -> Result<Self, McpError> {
        let mut child = Command::new(&config.command)
            .args(&config.args)
            .envs(config.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr is the server's log channel in the MCP stdio transport;
            // inherit it so server logs land in ours.
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| McpError::Spawn {
                command: config.command.clone(),
                source,
            })?;
        let stdin = child.stdin.take().expect("child stdin piped");
        let stdout = child.stdout.take().expect("child stdout piped");

        let pending: PendingMap = Arc::default();
        let reader = {
            let pending = pending.clone();
            let server = config.name.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    route_line(&server, &pending, &line);
                }
                // EOF or read error: fail every waiter by dropping its sender.
                pending.lock().expect("mcp pending lock poisoned").clear();
                tracing::debug!(server = %server, "MCP server stdout closed");
            })
        };

        let client = Self {
            server_name: config.name.clone(),
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            pending,
            next_id: AtomicU64::new(1),
            tools: RwLock::new(Vec::new()),
            reader,
        };

        client
            .request(
                "initialize",
                Some(protocol::initialize_params()),
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await?;
        client.notify("notifications/initialized", None).await?;

        let result = client
            .request("tools/list", None, DEFAULT_REQUEST_TIMEOUT)
            .await?;
        let list: ListToolsResult = serde_json::from_value(result)
            .map_err(|e| McpError::Protocol(format!("invalid tools/list result: {e}")))?;
        tracing::debug!(
            server = %client.server_name,
            tools = list.tools.len(),
            "MCP server connected"
        );
        *client.tools.write().expect("mcp tools lock poisoned") = list.tools;
        Ok(client)
    }

    /// The configured server name (used to namespace tool ids).
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// The tools the server advertised at connect time.
    pub fn list_tools(&self) -> Vec<ToolInfo> {
        self.tools.read().expect("mcp tools lock poisoned").clone()
    }

    /// Call a remote tool by its (un-namespaced) MCP name. Text content items
    /// are concatenated into the returned string; non-text items are noted as
    /// e.g. `[image content]`. A result with `isError: true` becomes
    /// [`McpError::ToolFailed`].
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<String, McpError> {
        let params = json!({ "name": name, "arguments": arguments });
        let result = self
            .request("tools/call", Some(params), CALL_TOOL_TIMEOUT)
            .await?;
        let call: CallToolResult = serde_json::from_value(result)
            .map_err(|e| McpError::Protocol(format!("invalid tools/call result: {e}")))?;
        let text = protocol::render_content(&call.content);
        if call.is_error {
            return Err(McpError::ToolFailed {
                name: name.to_string(),
                message: text,
            });
        }
        Ok(text)
    }

    /// Kill the server process and stop the reader task. Idempotent; also
    /// happens implicitly on drop via `kill_on_drop`.
    pub async fn shutdown(&self) {
        self.reader.abort();
        let mut child = self.child.lock().await;
        if let Err(e) = child.kill().await {
            tracing::debug!(server = %self.server_name, error = %e, "MCP server kill failed");
        }
        self.pending
            .lock()
            .expect("mcp pending lock poisoned")
            .clear();
    }

    /// Send a request and await its response (or time out).
    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // Encode BEFORE registering the pending sender so an encode failure
        // can't leave an orphaned entry in the pending map.
        let payload = serde_json::to_string(&Request::new(id, method, params))
            .map_err(|e| McpError::Protocol(format!("failed to encode `{method}`: {e}")))?;
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("mcp pending lock poisoned")
            .insert(id, tx);
        if let Err(e) = self.send_line(payload).await {
            self.remove_pending(id);
            return Err(e);
        }

        let response = match tokio::time::timeout(timeout, rx).await {
            Err(_elapsed) => {
                self.remove_pending(id);
                return Err(McpError::Timeout {
                    method: method.to_string(),
                    after_ms: timeout.as_millis(),
                });
            }
            // Sender dropped: the reader task saw EOF (server exited).
            Ok(Err(_recv)) => return Err(McpError::Closed),
            Ok(Ok(response)) => response,
        };
        if let Some(error) = response.error {
            return Err(McpError::Rpc {
                code: error.code,
                message: error.message,
            });
        }
        Ok(response.result.unwrap_or(Value::Null))
    }

    /// Send a notification (no response expected).
    async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        let payload = serde_json::to_string(&Notification::new(method, params))
            .map_err(|e| McpError::Protocol(format!("failed to encode `{method}`: {e}")))?;
        self.send_line(payload).await
    }

    /// Write one newline-terminated JSON-RPC message to the server's stdin.
    async fn send_line(&self, payload: String) -> Result<(), McpError> {
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(payload.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    fn remove_pending(&self, id: u64) {
        self.pending
            .lock()
            .expect("mcp pending lock poisoned")
            .remove(&id);
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Child is killed by kill_on_drop; just stop the reader task.
        self.reader.abort();
    }
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("server_name", &self.server_name)
            .finish_non_exhaustive()
    }
}

/// Route one line from the server's stdout: responses are matched to waiting
/// requests by id; anything else (server-initiated requests, notifications,
/// non-JSON noise) is logged and ignored.
fn route_line(
    server: &str,
    pending: &StdMutex<HashMap<u64, oneshot::Sender<Response>>>,
    line: &str,
) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(server = %server, error = %e, "ignoring non-JSON MCP output line");
            return;
        }
    };
    let is_response = value.get("id").is_some_and(Value::is_u64)
        && (value.get("result").is_some() || value.get("error").is_some());
    if !is_response {
        tracing::debug!(server = %server, "ignoring non-response MCP message");
        return;
    }
    match serde_json::from_value::<Response>(value) {
        Ok(response) => {
            let waiter = pending
                .lock()
                .expect("mcp pending lock poisoned")
                .remove(&response.id);
            match waiter {
                // The waiter may have timed out and gone; that's fine.
                Some(tx) => drop(tx.send(response)),
                None => {
                    tracing::debug!(server = %server, "MCP response for unknown/expired request id")
                }
            }
        }
        Err(e) => tracing::debug!(server = %server, error = %e, "malformed MCP response"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_with(id: u64) -> (PendingMap, oneshot::Receiver<Response>) {
        let pending: PendingMap = Arc::default();
        let (tx, rx) = oneshot::channel();
        pending.lock().expect("lock").insert(id, tx);
        (pending, rx)
    }

    #[test]
    fn route_line_delivers_matching_response() {
        let (pending, mut rx) = pending_with(42);
        route_line(
            "fake",
            &pending,
            r#"{"jsonrpc":"2.0","id":42,"result":{"ok":true}}"#,
        );
        let response = rx.try_recv().expect("response routed");
        assert_eq!(response.id, 42);
        assert_eq!(response.result.unwrap()["ok"], true);
        assert!(pending.lock().expect("lock").is_empty());
    }

    #[test]
    fn route_line_ignores_noise_and_notifications() {
        let (pending, mut rx) = pending_with(1);
        route_line("fake", &pending, "not json at all");
        route_line("fake", &pending, "");
        route_line(
            "fake",
            &pending,
            r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{}}"#,
        );
        // A response for a different id must not consume our waiter.
        route_line(
            "fake",
            &pending,
            r#"{"jsonrpc":"2.0","id":999,"result":{}}"#,
        );
        assert!(rx.try_recv().is_err(), "nothing should have been routed");
        assert_eq!(pending.lock().expect("lock").len(), 1);
    }
}
