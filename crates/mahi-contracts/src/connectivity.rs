//! The connectivity seam: the multiplexed bus envelope shared by all transports.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Logical channels multiplexed over one secure device-to-device connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum ChannelId {
    /// Session lifecycle, heartbeat, kill-switch. Highest priority.
    Control = 0x01,
    /// Streaming inference tokens / request-response.
    Inference = 0x02,
    /// Handoff state, task-queue, checkpoints.
    Sync = 0x03,
    /// Approval prompts + responses.
    Approval = 0x04,
    /// Mac takeover screen frames.
    StreamVideo = 0x05,
    /// Takeover input events.
    Input = 0x06,
    /// Chunked file transfer.
    FileXfr = 0x07,
    /// Reserved (audio forwarding, later).
    Audio = 0x08,
}

/// A framed message on the multiplexed bus. The payload is domain-owned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiplexedEnvelope {
    pub channel: ChannelId,
    pub seq_no: u64,
    pub session_id: Uuid,
    /// Logical type name of the payload, e.g. "InferenceChunk".
    pub payload_type: String,
    /// Serialized payload bytes (JSON in Phase 0).
    pub payload_bytes: Vec<u8>,
}
