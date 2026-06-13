//! MCP (Model Context Protocol) client support.
//!
//! Mahi connects to external MCP servers over the stdio transport (the
//! standard "command + args" configuration, identical in shape to Claude
//! Desktop's `mcpServers` config) and exposes every tool those servers
//! advertise as a regular registry tool with the namespaced id
//! `mcp__<server>__<tool>`, so the agent core can call them like built-ins.
//!
//! Layout:
//! - [`protocol`]: hand-rolled minimal JSON-RPC 2.0 + MCP wire types (no SDK).
//! - [`client`]: [`McpClient`] — spawns the server process, speaks
//!   newline-delimited JSON-RPC over its stdin/stdout, caches `tools/list`.
//! - [`config`]: [`McpServerConfig`] + [`load_mcp_config`] for the
//!   Claude-Desktop-compatible `mcp_servers.json` file shape.
//! - [`adapter`]: [`McpToolAdapter`] — presents one remote MCP tool as an
//!   internal [`crate::tool::Tool`]; registered via
//!   [`crate::ToolRegistry::attach_mcp_server`].
//!
//! ## Safety model
//!
//! MCP servers are external, unaudited child processes: every MCP tool
//! descriptor carries `requires_approval = true` and
//! `destructive_level = High` (mirroring how `shell_exec` is approval-gated),
//! and MCP tools are only offered in the modes where a Mac executes tools
//! locally (`MacLan` / `MacRemote`), like the shell tool.

pub mod adapter;
pub mod client;
pub mod config;
pub mod protocol;

pub use adapter::McpToolAdapter;
pub use client::McpClient;
pub use config::{load_mcp_config, parse_mcp_config, McpServerConfig};
pub use protocol::ToolInfo;

use thiserror::Error;

/// Errors from the MCP subsystem (config, transport, protocol, tool calls).
#[derive(Debug, Error)]
pub enum McpError {
    /// The server process could not be spawned.
    #[error("failed to spawn MCP server command `{command}`: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    /// I/O failure on the stdio transport.
    #[error("MCP transport I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The server sent something that is not valid MCP/JSON-RPC.
    #[error("MCP protocol error: {0}")]
    Protocol(String),
    /// The server answered with a JSON-RPC error object.
    #[error("MCP server returned JSON-RPC error {code}: {message}")]
    Rpc { code: i64, message: String },
    /// No response arrived in time.
    #[error("MCP request `{method}` timed out after {after_ms} ms")]
    Timeout { method: String, after_ms: u128 },
    /// The server exited or closed its stdout before responding.
    #[error("MCP connection closed by server")]
    Closed,
    /// `tools/call` succeeded at the protocol level but reported
    /// `isError: true`.
    #[error("MCP tool `{name}` reported an error: {message}")]
    ToolFailed { name: String, message: String },
    /// The `mcp_servers.json` config could not be read or parsed.
    #[error("invalid MCP config: {0}")]
    Config(String),
}

impl McpError {
    /// Whether retrying the same call could plausibly succeed (transport-level
    /// failures) as opposed to deterministic protocol/config/tool errors.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            McpError::Io(_) | McpError::Timeout { .. } | McpError::Closed
        )
    }
}

/// The registry id for a tool advertised by an MCP server:
/// `mcp__<server>__<tool>`.
pub fn mcp_tool_id(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_tool_ids() {
        assert_eq!(
            mcp_tool_id("github", "create_issue"),
            "mcp__github__create_issue"
        );
        assert_eq!(mcp_tool_id("fake", "echo"), "mcp__fake__echo");
    }

    #[test]
    fn classifies_retryable_errors() {
        assert!(McpError::Closed.retryable());
        assert!(McpError::Timeout {
            method: "tools/call".into(),
            after_ms: 1
        }
        .retryable());
        assert!(!McpError::Rpc {
            code: -32601,
            message: "method not found".into()
        }
        .retryable());
        assert!(!McpError::Config("bad".into()).retryable());
        assert!(!McpError::ToolFailed {
            name: "echo".into(),
            message: "boom".into()
        }
        .retryable());
    }
}
