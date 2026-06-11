//! The internal tool abstraction every built-in, computer-use, and
//! connector-backed tool implements, plus small stream-building helpers.

use async_trait::async_trait;
use mahi_contracts::error::ContractError;
use mahi_contracts::tooling::{ToolDescriptor, ToolEvent, ToolEventStream};
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

/// One executable tool. The [`crate::ToolRegistry`] owns a set of these and
/// dispatches [`mahi_contracts::tooling::ToolInvocation`]s to them.
///
/// Custom tools implement this trait and are added via
/// [`crate::ToolRegistry::register`].
#[async_trait]
pub trait Tool: Send + Sync {
    /// Static metadata for this tool (id, modes, approval requirements, ...).
    fn descriptor(&self) -> ToolDescriptor;

    /// Execute the tool. Argument validation failures and runtime failures
    /// should be reported in-stream (as [`ToolEvent::Error`] or an `Err`
    /// item) rather than panicking. The registry additionally wraps the
    /// returned stream with cancellation handling, but long-running tools
    /// should also observe `cancel` themselves to stop underlying work
    /// (e.g. kill a child process).
    async fn run(&self, args: serde_json::Value, cancel: CancellationToken) -> ToolEventStream;
}

/// Build a stream from an already-materialized list of events.
pub fn events(items: Vec<Result<ToolEvent, ContractError>>) -> ToolEventStream {
    Box::pin(futures::stream::iter(items))
}

/// A single successful, non-truncated [`ToolEvent::Result`].
pub fn ok_result(output: serde_json::Value) -> ToolEventStream {
    events(vec![Ok(ToolEvent::Result { output, truncated: false })])
}

/// A single in-stream [`ToolEvent::Error`].
pub fn tool_error(message: impl Into<String>, retryable: bool) -> ToolEventStream {
    events(vec![Ok(ToolEvent::Error { message: message.into(), retryable })])
}

/// A single stream item carrying a hard contract error.
pub fn contract_error(err: ContractError) -> ToolEventStream {
    events(vec![Err(err)])
}

/// Adapt a bounded mpsc receiver into a [`ToolEventStream`]; used by tools
/// that stream incrementally (e.g. `shell_exec`).
pub fn channel_stream(
    rx: tokio::sync::mpsc::Receiver<Result<ToolEvent, ContractError>>,
) -> ToolEventStream {
    Box::pin(futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
}

/// Deserialize tool args, mapping failures to a human-readable message
/// suitable for a [`ToolEvent::Error`].
pub(crate) fn parse_args<T: DeserializeOwned>(args: serde_json::Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("input schema validation failed: {e}"))
}
