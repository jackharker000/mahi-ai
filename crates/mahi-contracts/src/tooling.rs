//! The tooling seam: how the agent core invokes capabilities uniformly.

use crate::error::ContractError;
use crate::types::ComputeMode;
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Static metadata describing one tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub id: String,
    pub display_name: String,
    pub category: ToolCategory,
    pub available_in_modes: Vec<ComputeMode>,
    pub required_permissions: Vec<String>,
    pub input_schema: serde_json::Value,
    pub output_schema: serde_json::Value,
    pub requires_approval: bool,
    pub destructive_level: DestructiveLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolCategory {
    BuiltIn,
    Connector,
    CodingAgent,
    ComputerUse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DestructiveLevel {
    Low,
    High,
    Critical,
}

/// A request to invoke a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInvocation {
    pub invocation_id: Uuid,
    pub tool_id: String,
    pub args: serde_json::Value,
    pub session_id: Uuid,
    pub conversation_id: Uuid,
    pub message_id: Uuid,
    pub compute_mode: ComputeMode,
    pub trace_id: Uuid,
    pub stream: bool,
}

/// A stream of events emitted while a tool runs.
pub type ToolEventStream = Pin<Box<dyn Stream<Item = Result<ToolEvent, ContractError>> + Send>>;

/// One event in a tool's lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToolEvent {
    Chunk { data: String },
    Citation { url: String, title: Option<String>, excerpt: Option<String> },
    ApprovalRequired { approval_id: Uuid, summary: String, destructive_level: DestructiveLevel },
    Result { output: serde_json::Value, truncated: bool },
    Error { message: String, retryable: bool },
    Cancelled,
}

/// The uniform contract the agent core uses to discover and run tools.
#[async_trait]
pub trait ToolInvokeContract: Send + Sync {
    /// List the tools available in a given compute mode.
    async fn describe(&self, mode: ComputeMode) -> Vec<ToolDescriptor>;

    /// Invoke a tool, returning a stream of events.
    async fn invoke(
        &self,
        call: ToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolEventStream, ContractError>;
}
