//! The inference seam: how the agent core talks to compute (modes A–D).

use crate::data::Message;
use crate::error::ContractError;
use crate::types::{CapabilitySet, ComputeMode, ModelDescriptor};
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// A stream of inference chunks produced by a provider.
pub type InferenceStream = Pin<Box<dyn Stream<Item = Result<InferenceChunk, ContractError>> + Send>>;

/// A single inference request, mode-agnostic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceRequest {
    pub request_id: Uuid,
    pub messages: Vec<Message>,
    pub tools: Option<Vec<ToolSpec>>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub streaming_hint: bool,
    pub required_caps: CapabilitySet,
}

impl InferenceRequest {
    /// Build a minimal text-only request from a list of messages.
    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self {
            request_id: Uuid::new_v4(),
            messages,
            tools: None,
            max_tokens: None,
            temperature: None,
            streaming_hint: true,
            required_caps: CapabilitySet::none(),
        }
    }
}

/// A tool definition handed to a provider for tool-calling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub id: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// One streamed unit of model output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceChunk {
    pub delta: Option<String>,
    pub tool_call_delta: Option<ToolCallDelta>,
    pub finish_reason: Option<FinishReason>,
    /// Echoed on *every* chunk so the surface always knows the active mode.
    pub active_mode: ComputeMode,
    pub latency_hint_ms: Option<u32>,
}

impl InferenceChunk {
    /// A pure text delta chunk for `mode`.
    pub fn text(delta: impl Into<String>, mode: ComputeMode) -> Self {
        Self {
            delta: Some(delta.into()),
            tool_call_delta: None,
            finish_reason: None,
            active_mode: mode,
            latency_hint_ms: None,
        }
    }

    /// A terminal chunk carrying a finish reason.
    pub fn finish(reason: FinishReason, mode: ComputeMode) -> Self {
        Self {
            delta: None,
            tool_call_delta: None,
            finish_reason: Some(reason),
            active_mode: mode,
            latency_hint_ms: None,
        }
    }
}

/// An incremental tool-call payload; accumulate `args_delta` until `finish_reason`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallDelta {
    pub call_id: String,
    pub tool_id: String,
    pub args_delta: String,
}

/// Why a generation stream ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FinishReason {
    Stop,
    ToolCall,
    MaxTokens,
    Cancelled,
    Error,
}

/// The result of probing whether a provider can serve a request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanHandleResult {
    pub capable: bool,
    pub missing_caps: CapabilitySet,
    pub escalation_hint: Option<ComputeMode>,
}

impl CanHandleResult {
    /// A "yes, fully capable" result.
    pub fn capable() -> Self {
        Self { capable: true, missing_caps: CapabilitySet::none(), escalation_hint: None }
    }
}

/// A source of model inference. One impl per mode (on-device, Mac, hosted).
#[async_trait]
pub trait InferenceProvider: Send + Sync {
    /// Static description of the model this provider serves.
    fn descriptor(&self) -> ModelDescriptor;

    /// Probe (no side effects) whether this provider can serve `req`.
    async fn can_handle(&self, req: &InferenceRequest) -> CanHandleResult;

    /// Begin streaming generation. The caller may halt it via `cancel`.
    async fn generate(
        &self,
        req: InferenceRequest,
        cancel: CancellationToken,
    ) -> Result<InferenceStream, ContractError>;
}
