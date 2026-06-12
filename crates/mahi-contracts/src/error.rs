//! The cross-domain error taxonomy.

use crate::types::ComputeMode;
use thiserror::Error;

/// Top-level error returned across every contract boundary.
#[derive(Debug, Error)]
pub enum ContractError {
    #[error("inference error: {0}")]
    Inference(#[from] InferenceError),
    #[error("tool error: {0}")]
    Tool(#[from] ToolError),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("connectivity error: {0}")]
    Connectivity(#[from] ConnectivityError),
    #[error("permission error: {0}")]
    Permission(#[from] PermissionError),
    #[error("operation cancelled")]
    Cancelled,
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Error)]
pub enum InferenceError {
    #[error("no provider can handle the request")]
    NoCapableProvider,
    #[error("pin violation: requested mode {requested:?} is unavailable")]
    PinViolation { requested: ComputeMode },
    #[error("provider error: {message} (retryable={retryable})")]
    Provider { message: String, retryable: bool },
    #[error("compute resource is thermally throttled")]
    ThermalThrottle,
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool not found: {tool_id}")]
    NotFound { tool_id: String },
    #[error("tool {tool_id} unavailable in mode {mode:?}")]
    UnavailableInMode { tool_id: String, mode: ComputeMode },
    #[error("input schema validation failed: {message}")]
    SchemaValidation { message: String },
    #[error("sandbox violation")]
    SandboxViolation,
    #[error("tool execution failed: {message}")]
    Execution { message: String },
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("record not found: {id}")]
    NotFound { id: uuid::Uuid },
    #[error("storage backend error: {message}")]
    Backend { message: String },
    #[error("encryption error: {message}")]
    Encryption { message: String },
    #[error("serialization error: {message}")]
    Serialization { message: String },
}

#[derive(Debug, Error)]
pub enum ConnectivityError {
    #[error("peer unreachable: {peer_id}")]
    PeerUnreachable { peer_id: uuid::Uuid },
    #[error("channel {channel:?} is not open")]
    ChannelClosed {
        channel: crate::connectivity::ChannelId,
    },
    #[error("transport error: {message}")]
    Transport { message: String },
}

#[derive(Debug, Error)]
pub enum PermissionError {
    #[error("permission denied for {subject} in scope {scope:?}")]
    Denied { subject: String, scope: Vec<String> },
    #[error("blocked by hard rule: {rule_name}")]
    HardRuleBlocked { rule_name: String },
    #[error("backend error: {message}")]
    Backend { message: String },
}

impl ContractError {
    /// Convenience constructor for ad-hoc errors.
    pub fn other(msg: impl Into<String>) -> Self {
        ContractError::Other(msg.into())
    }
}
