//! Connector broker skeleton (MCP-class).
//!
//! A [`Connector`] is an external capability provider that contributes
//! [`ToolDescriptor`]s and handles invokes for them. The registry mounts a
//! connector via [`crate::ToolRegistry::mount_connector`], which wraps each
//! contributed descriptor in an internal adapter so connector tools dispatch
//! exactly like built-ins.
//!
//! Deliberately light for phase 0: no transport (StdIO/HTTP/SSE), manifest
//! signing, auth schemes, or permission tiers yet — see
//! `docs/backend/domains/02-tooling-integrations.md` §3 for the full
//! `ConnectorManifest` design those will plug into.

use crate::tool::{ok_result, parse_args, tool_error, Tool};
use async_trait::async_trait;
use mahi_contracts::tooling::{DestructiveLevel, ToolCategory, ToolDescriptor, ToolEventStream};
use mahi_contracts::types::ComputeMode;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// An external (MCP-class) capability provider.
#[async_trait]
pub trait Connector: Send + Sync {
    /// Stable connector id, e.g. "github". Used to namespace tool ids.
    fn id(&self) -> &str;

    /// Human-readable name for capability UIs.
    fn display_name(&self) -> &str;

    /// Tool descriptors this connector contributes. Ids should be namespaced
    /// `"<connector_id>.<tool_name>"` and must be globally unique within a
    /// registry; `category` should be [`ToolCategory::Connector`].
    fn tools(&self) -> Vec<ToolDescriptor>;

    /// Handle an invocation of one of this connector's tools.
    async fn invoke(
        &self,
        tool_id: &str,
        args: serde_json::Value,
        cancel: CancellationToken,
    ) -> ToolEventStream;
}

/// Adapter: presents one connector-contributed descriptor as a [`Tool`].
pub(crate) struct ConnectorTool {
    descriptor: ToolDescriptor,
    connector: Arc<dyn Connector>,
}

impl ConnectorTool {
    pub(crate) fn new(descriptor: ToolDescriptor, connector: Arc<dyn Connector>) -> Self {
        Self { descriptor, connector }
    }
}

#[async_trait]
impl Tool for ConnectorTool {
    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    async fn run(&self, args: serde_json::Value, cancel: CancellationToken) -> ToolEventStream {
        self.connector.invoke(&self.descriptor.id, args, cancel).await
    }
}

/// Trivial example connector: contributes one `echo.say` tool that returns
/// its message back. Useful as a wiring reference and in tests.
#[derive(Debug, Default)]
pub struct EchoConnector;

impl EchoConnector {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Deserialize)]
struct EchoArgs {
    message: String,
}

#[async_trait]
impl Connector for EchoConnector {
    fn id(&self) -> &str {
        "echo"
    }

    fn display_name(&self) -> &str {
        "Echo (example connector)"
    }

    fn tools(&self) -> Vec<ToolDescriptor> {
        vec![ToolDescriptor {
            id: "echo.say".to_string(),
            display_name: "Echo".to_string(),
            category: ToolCategory::Connector,
            available_in_modes: vec![
                ComputeMode::OnDevice,
                ComputeMode::MacLan,
                ComputeMode::MacRemote,
                ComputeMode::Hosted,
            ],
            required_permissions: vec![],
            input_schema: json!({
                "type": "object",
                "properties": { "message": { "type": "string" } },
                "required": ["message"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": { "echo": { "type": "string" } }
            }),
            requires_approval: false,
            destructive_level: DestructiveLevel::Low,
        }]
    }

    async fn invoke(
        &self,
        tool_id: &str,
        args: serde_json::Value,
        _cancel: CancellationToken,
    ) -> ToolEventStream {
        match tool_id {
            "echo.say" => match parse_args::<EchoArgs>(args) {
                Ok(a) => ok_result(json!({ "echo": a.message })),
                Err(msg) => tool_error(msg, false),
            },
            other => tool_error(format!("echo connector has no tool `{other}`"), false),
        }
    }
}
