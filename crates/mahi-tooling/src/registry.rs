//! [`ToolRegistry`]: the default [`ToolInvokeContract`] implementation.
//!
//! Single source of truth for `describe()` (per-mode capability queries) and
//! the dispatcher for `invoke()`. Built-ins, computer-use tools, and mounted
//! connector tools all dispatch through the same internal [`Tool`] trait.

use crate::computer::ComputerController;
use crate::connector::{Connector, ConnectorTool};
use crate::tool::Tool;
use crate::tools::apple_script::AppleScriptTool;
use crate::tools::computer_use::{
    ScreenCaptureTool, UiClickTool, UiDescribeTool, UiKeyTool, UiScrollTool, UiTypeTool,
};
use crate::tools::file::{FileEditTool, FileReadTool, FileScope, FileSearchTool, FileWriteTool};
use crate::tools::run_code::RunCodeTool;
use crate::tools::shell::ShellExecTool;
use crate::tools::web::{WebFetchTool, WebSearchTool};
use async_trait::async_trait;
use futures::StreamExt;
use mahi_contracts::error::{ContractError, ToolError};
use mahi_contracts::tooling::{
    ToolDescriptor, ToolEvent, ToolEventStream, ToolInvocation, ToolInvokeContract,
};
use mahi_contracts::types::ComputeMode;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tokio_util::sync::CancellationToken;

/// The default tool registry implementing [`ToolInvokeContract`].
pub struct ToolRegistry {
    /// BTreeMap keeps `describe()` output deterministically ordered by id.
    tools: RwLock<BTreeMap<String, Arc<dyn Tool>>>,
    /// Mounted connectors, kept for capability/inventory queries.
    connectors: RwLock<Vec<Arc<dyn Connector>>>,
}

impl ToolRegistry {
    /// A registry with no tools. Add tools via [`Self::register`] or
    /// [`Self::mount_connector`].
    pub fn empty() -> Self {
        Self {
            tools: RwLock::new(BTreeMap::new()),
            connectors: RwLock::new(Vec::new()),
        }
    }

    /// Registry with the standard built-in tools wired to `controller` for
    /// computer use. File and shell tools are scoped to the process's current
    /// working directory; use [`Self::with_builtins_scoped`] to pick the root
    /// explicitly (recommended — the host app should pass the user-approved
    /// workspace directory).
    pub fn with_builtins(controller: Arc<dyn ComputerController>) -> Self {
        let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        Self::with_builtins_scoped(controller, root)
    }

    /// Registry with the standard built-ins, with file/shell access confined
    /// to `allowed_root`.
    pub fn with_builtins_scoped(
        controller: Arc<dyn ComputerController>,
        allowed_root: impl AsRef<Path>,
    ) -> Self {
        let registry = Self::empty();
        let scope = FileScope::new(allowed_root.as_ref());

        let builtins: Vec<Arc<dyn Tool>> = vec![
            Arc::new(FileReadTool::new(scope.clone())),
            Arc::new(FileWriteTool::new(scope.clone())),
            Arc::new(FileEditTool::new(scope.clone())),
            Arc::new(FileSearchTool::new(scope.clone())),
            Arc::new(ShellExecTool::new(scope)),
            Arc::new(AppleScriptTool::new()),
            Arc::new(RunCodeTool::new()),
            Arc::new(WebFetchTool),
            Arc::new(WebSearchTool),
            Arc::new(ScreenCaptureTool::new(controller.clone())),
            Arc::new(UiDescribeTool::new(controller.clone())),
            Arc::new(UiClickTool::new(controller.clone())),
            Arc::new(UiTypeTool::new(controller.clone())),
            Arc::new(UiScrollTool::new(controller.clone())),
            Arc::new(UiKeyTool::new(controller)),
        ];
        for tool in builtins {
            registry
                .register(tool)
                .expect("built-in tool ids are unique");
        }
        registry
    }

    /// Register a custom tool. Fails if a tool with the same id exists.
    pub fn register(&self, tool: Arc<dyn Tool>) -> Result<(), ContractError> {
        let id = tool.descriptor().id;
        let mut tools = self.tools.write().expect("tool registry lock poisoned");
        if tools.contains_key(&id) {
            return Err(ContractError::other(format!(
                "tool `{id}` is already registered"
            )));
        }
        tools.insert(id, tool);
        Ok(())
    }

    /// Mount a connector: every descriptor it contributes becomes a
    /// registered tool dispatching back into the connector.
    pub fn mount_connector(&self, connector: Arc<dyn Connector>) -> Result<(), ContractError> {
        let descriptors = connector.tools();
        // Register all-or-nothing so a half-mounted connector never lingers.
        {
            let tools = self.tools.read().expect("tool registry lock poisoned");
            for descriptor in &descriptors {
                if tools.contains_key(&descriptor.id) {
                    return Err(ContractError::other(format!(
                        "connector `{}` contributes tool `{}` which is already registered",
                        connector.id(),
                        descriptor.id
                    )));
                }
            }
        }
        for descriptor in descriptors {
            self.register(Arc::new(ConnectorTool::new(descriptor, connector.clone())))?;
        }
        self.connectors
            .write()
            .expect("connector list lock poisoned")
            .push(connector);
        Ok(())
    }

    /// Ids of all mounted connectors, in mount order.
    pub fn connector_ids(&self) -> Vec<String> {
        self.connectors
            .read()
            .expect("connector list lock poisoned")
            .iter()
            .map(|c| c.id().to_string())
            .collect()
    }

    /// Register every tool a connected [`McpClient`] advertised as a namespaced
    /// (`mcp__<server>__<tool>`) registry tool. The client is shared across all
    /// of its adapters, so dropping the registry tears the server down.
    ///
    /// All-or-nothing: if any namespaced id already exists nothing is
    /// registered. Returns the registered tool ids on success.
    pub fn attach_mcp_server(
        &self,
        client: Arc<crate::mcp::McpClient>,
    ) -> Result<Vec<String>, ContractError> {
        let server = client.server_name().to_string();
        let tools = client.list_tools();
        let ids: Vec<String> = tools
            .iter()
            .map(|t| crate::mcp::mcp_tool_id(&server, &t.name))
            .collect();
        {
            let existing = self.tools.read().expect("tool registry lock poisoned");
            for id in &ids {
                if existing.contains_key(id) {
                    return Err(ContractError::other(format!(
                        "MCP server `{server}` contributes tool `{id}` which is already registered"
                    )));
                }
            }
        }
        for tool in &tools {
            self.register(Arc::new(crate::mcp::McpToolAdapter::new(
                Arc::clone(&client),
                tool,
            )))?;
        }
        Ok(ids)
    }

    /// Load a Claude-Desktop-style `mcpServers` config file, connect to every
    /// server, and mount their tools. Resilient: a server that fails to start
    /// or whose tools collide is logged and skipped rather than failing the
    /// whole load. Returns every successfully registered tool id. A missing
    /// config file is not an error (returns an empty list).
    pub async fn mount_mcp_servers_from_config(
        &self,
        config_path: impl AsRef<Path>,
    ) -> Result<Vec<String>, ContractError> {
        let configs = crate::mcp::load_mcp_config(config_path.as_ref())
            .map_err(|e| ContractError::other(format!("failed to read MCP config: {e}")))?;
        let mut all_ids = Vec::new();
        for cfg in configs {
            match crate::mcp::McpClient::connect(&cfg).await {
                Ok(client) => match self.attach_mcp_server(Arc::new(client)) {
                    Ok(ids) => all_ids.extend(ids),
                    Err(e) => {
                        tracing::warn!(server = %cfg.name, error = %e, "MCP server tools not attached")
                    }
                },
                Err(e) => {
                    tracing::warn!(server = %cfg.name, error = %e, "MCP server failed to start")
                }
            }
        }
        Ok(all_ids)
    }
}

#[async_trait]
impl ToolInvokeContract for ToolRegistry {
    async fn describe(&self, mode: ComputeMode) -> Vec<ToolDescriptor> {
        self.tools
            .read()
            .expect("tool registry lock poisoned")
            .values()
            .map(|tool| tool.descriptor())
            .filter(|descriptor| descriptor.available_in_modes.contains(&mode))
            .collect()
    }

    async fn invoke(
        &self,
        call: ToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolEventStream, ContractError> {
        let tool = {
            let tools = self.tools.read().expect("tool registry lock poisoned");
            tools
                .get(&call.tool_id)
                .cloned()
                .ok_or_else(|| ToolError::NotFound {
                    tool_id: call.tool_id.clone(),
                })?
        };
        let descriptor = tool.descriptor();
        if !descriptor.available_in_modes.contains(&call.compute_mode) {
            return Err(ToolError::UnavailableInMode {
                tool_id: call.tool_id.clone(),
                mode: call.compute_mode,
            }
            .into());
        }
        // NOTE: approval gating is enforced by the agent core, which reads
        // `descriptor.requires_approval` and emits AgentEvent/ToolEvent
        // approval flows before calling invoke().
        // TODO(contracts): ToolInvokeContract has no "resume after approval"
        // seam, so the registry cannot enforce approvals itself.
        tracing::debug!(
            tool_id = %call.tool_id,
            invocation_id = %call.invocation_id,
            trace_id = %call.trace_id,
            mode = ?call.compute_mode,
            "dispatching tool invocation"
        );
        let stream = tool.run(call.args, cancel.clone()).await;
        Ok(with_cancellation(stream, cancel))
    }
}

/// Wrap a tool's event stream so cancellation deterministically ends it with
/// a single [`ToolEvent::Cancelled`].
fn with_cancellation(inner: ToolEventStream, cancel: CancellationToken) -> ToolEventStream {
    Box::pin(futures::stream::unfold(
        Some((inner, cancel)),
        |state| async move {
            let (mut inner, cancel) = state?;
            if cancel.is_cancelled() {
                return Some((Ok(ToolEvent::Cancelled), None));
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Some((Ok(ToolEvent::Cancelled), None)),
                item = inner.next() => item.map(|item| (item, Some((inner, cancel)))),
            }
        },
    ))
}
