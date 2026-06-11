//! Events emitted by the agent core to a surface (UI/CLI/FFI).

use crate::compute::FinishReason;
use crate::tooling::ToolEvent;
use crate::types::ComputeMode;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A single event in an agent turn, streamed to the surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AgentEvent {
    /// A turn has begun.
    TurnStarted {
        conversation_id: Uuid,
        message_id: Uuid,
        mode: ComputeMode,
    },
    /// A chunk of assistant text.
    TextDelta { text: String },
    /// A tool produced an event.
    Tool { event: ToolEvent },
    /// The active compute mode changed mid-turn (handoff).
    ModeHandoff { from: ComputeMode, to: ComputeMode },
    /// An approval is required before continuing.
    ApprovalRequired { approval_id: Uuid, summary: String },
    /// The turn finished.
    TurnFinished { reason: FinishReason },
    /// A non-fatal error occurred.
    Error { message: String },
}
