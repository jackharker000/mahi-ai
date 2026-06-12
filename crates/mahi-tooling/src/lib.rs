//! # mahi-tooling
//!
//! The tool surface of Mahi AI: the default [`ToolRegistry`] implementing
//! [`mahi_contracts::tooling::ToolInvokeContract`], the built-in tools
//! (filesystem, shell, web, Claude-style computer use), the
//! [`ComputerController`] platform hook, and a light connector broker.
//!
//! Design references:
//! - `docs/backend/phase-0/03-engine-facade.md` §"mahi-tooling registry"
//! - `docs/backend/domains/02-tooling-integrations.md`
//!
//! ## Quick start
//!
//! ```
//! use std::sync::Arc;
//! use mahi_tooling::{MockComputerController, ToolRegistry};
//!
//! let controller = Arc::new(MockComputerController::new());
//! let registry = ToolRegistry::with_builtins(controller);
//! // registry implements mahi_contracts::tooling::ToolInvokeContract:
//! // registry.describe(mode).await / registry.invoke(call, cancel).await
//! ```
//!
//! ## Safety model
//!
//! - File and shell tools are confined to an allowed root directory
//!   ([`ToolRegistry::with_builtins_scoped`]); escapes — including via
//!   symlink — are `ToolError::SandboxViolation`.
//! - Mutating tools (`file_write`, `file_edit`, `shell_exec`, `ui_click`,
//!   `ui_type`, `ui_key`) carry `requires_approval = true` in their
//!   descriptors; the agent core surfaces the approval wall.
//! - Computer-use input tools additionally refuse outright when
//!   [`ComputerController::is_sensitive_context`] reports a login/payment
//!   surface, so approvals cannot leak credentials.

pub mod computer;
pub mod connector;
pub mod mcp;
pub mod registry;
pub mod tool;
pub mod tools;

pub use computer::{
    ComputerController, ControllerAction, MockComputerController, MouseButton, Screenshot,
    UiBounds, UiElement,
};
pub use connector::{Connector, EchoConnector};
pub use mcp::{load_mcp_config, McpClient, McpError, McpServerConfig};
pub use registry::ToolRegistry;
pub use tool::Tool;

// Re-export the contract seam types tool authors need, so downstream crates
// can depend on mahi-tooling alone for custom tools.
pub use mahi_contracts::tooling::{
    DestructiveLevel, ToolCategory, ToolDescriptor, ToolEvent, ToolEventStream, ToolInvocation,
    ToolInvokeContract,
};
