//! [`McpToolAdapter`]: presents one remote MCP tool as an internal [`Tool`].
//!
//! Each adapter wraps a shared [`McpClient`] and one advertised
//! [`ToolInfo`]; its registry id is `mcp__<server>__<tool>` and its
//! `input_schema` is the server's own JSON Schema, passed straight through to
//! the model. Invocation forwards the model's arguments to `tools/call` and
//! returns the flattened text content as the tool result.

use crate::mcp::protocol::ToolInfo;
use crate::mcp::{mcp_tool_id, McpClient};
use crate::tool::Tool;
use async_trait::async_trait;
use mahi_contracts::tooling::{
    DestructiveLevel, ToolCategory, ToolDescriptor, ToolEvent, ToolEventStream,
};
use mahi_contracts::types::ComputeMode;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// The modes an MCP tool is offered in. Mirrors `shell_exec`: MCP servers are
/// external child processes that execute where a Mac runs tools locally.
const MCP_MODES: [ComputeMode; 2] = [ComputeMode::MacLan, ComputeMode::MacRemote];

/// Adapts a single MCP server tool into the registry's [`Tool`] trait.
pub struct McpToolAdapter {
    client: Arc<McpClient>,
    /// The remote (un-namespaced) tool name to pass to `tools/call`.
    remote_name: String,
    descriptor: ToolDescriptor,
}

impl McpToolAdapter {
    /// Build an adapter for `tool` advertised by `client`'s server.
    pub fn new(client: Arc<McpClient>, tool: &ToolInfo) -> Self {
        let server = client.server_name().to_string();
        let id = mcp_tool_id(&server, &tool.name);
        let display_name = match &tool.description {
            Some(desc) if !desc.is_empty() => desc.lines().next().unwrap_or(&tool.name).to_string(),
            _ => format!("{server}: {}", tool.name),
        };
        let descriptor = ToolDescriptor {
            id,
            display_name,
            category: ToolCategory::Connector,
            available_in_modes: MCP_MODES.to_vec(),
            required_permissions: vec![format!("mcp:{server}")],
            input_schema: tool.input_schema.clone(),
            // MCP tool results are flattened to a single text string.
            output_schema: json!({
                "type": "object",
                "properties": { "content": { "type": "string" } }
            }),
            // External, unaudited code: always gated behind the approval wall,
            // like shell_exec.
            requires_approval: true,
            destructive_level: DestructiveLevel::High,
        };
        Self {
            client,
            remote_name: tool.name.clone(),
            descriptor,
        }
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    async fn run(&self, args: Value, cancel: CancellationToken) -> ToolEventStream {
        let client = Arc::clone(&self.client);
        let name = self.remote_name.clone();
        // MCP `arguments` must be an object; a model that supplies no args
        // sends JSON null, which we normalize to `{}`.
        let arguments = if args.is_null() { json!({}) } else { args };

        // Lazy single-item stream so the registry's cancellation wrapper can
        // drop the in-flight call; also observe `cancel` directly for promptness.
        Box::pin(futures::stream::once(async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Ok(ToolEvent::Cancelled),
                result = client.call_tool(&name, arguments) => match result {
                    Ok(text) => Ok(ToolEvent::Result {
                        output: json!({ "content": text }),
                        truncated: false,
                    }),
                    Err(err) => Ok(ToolEvent::Error {
                        message: err.to_string(),
                        retryable: err.retryable(),
                    }),
                },
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::protocol::ToolInfo;

    fn tool_info(name: &str, desc: Option<&str>) -> ToolInfo {
        ToolInfo {
            name: name.to_string(),
            description: desc.map(str::to_string),
            input_schema: json!({ "type": "object", "required": ["path"] }),
        }
    }

    // A descriptor can be built without a live server by constructing the
    // adapter's pieces directly; we exercise the id/mode/schema mapping that
    // does not need the client.
    #[test]
    fn descriptor_fields_map_from_tool_info() {
        // Build the descriptor the same way `new` does, without spawning a
        // process (McpClient construction needs a child; the pure mapping is
        // what we assert here).
        let info = tool_info("read_file", Some("Read a file from disk\nsecond line"));
        let server = "filesystem";
        let id = mcp_tool_id(server, &info.name);
        assert_eq!(id, "mcp__filesystem__read_file");

        // The display name is the first line of the description when present.
        let display = info
            .description
            .as_deref()
            .and_then(|d| d.lines().next())
            .unwrap();
        assert_eq!(display, "Read a file from disk");

        // Modes mirror shell_exec.
        assert_eq!(
            MCP_MODES.to_vec(),
            vec![ComputeMode::MacLan, ComputeMode::MacRemote]
        );
        // The remote schema is passed through verbatim.
        assert_eq!(info.input_schema["required"][0], "path");
    }
}
