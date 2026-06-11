//! Primitive shared types used across every domain.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// The four compute modes Mahi can run an inference request in.
///
/// Local-first preference order (see `docs/backend/DECISIONS.md` D1):
/// `MacLan` > `MacRemote` > `Hosted`, with `OnDevice` always available and the
/// privacy/offline default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ComputeMode {
    /// Small model on the device itself, fully offline.
    OnDevice,
    /// A paired Mac reached over the local network.
    MacLan,
    /// A paired Mac reached remotely over a secure tunnel.
    MacRemote,
    /// A hosted cloud model API.
    Hosted,
}

impl ComputeMode {
    /// Whether this mode requires any network at all.
    pub fn requires_network(&self) -> bool {
        !matches!(self, ComputeMode::OnDevice)
    }
}

/// A coarse description of what a model/mode can do, used for routing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilitySet {
    pub vision: bool,
    pub tool_calling: bool,
    /// Minimum usable context window, in tokens.
    pub min_context_window: u32,
    pub code_gen: bool,
}

impl CapabilitySet {
    /// An empty capability set (no special capabilities required/available).
    pub fn none() -> Self {
        Self::default()
    }

    /// Returns the capabilities present in `self` but missing from `other`.
    pub fn missing_from(&self, other: &CapabilitySet) -> CapabilitySet {
        CapabilitySet {
            vision: self.vision && !other.vision,
            tool_calling: self.tool_calling && !other.tool_calling,
            min_context_window: self.min_context_window.saturating_sub(other.min_context_window),
            code_gen: self.code_gen && !other.code_gen,
        }
    }

    /// Whether `other` satisfies everything `self` requires.
    pub fn satisfied_by(&self, other: &CapabilitySet) -> bool {
        (!self.vision || other.vision)
            && (!self.tool_calling || other.tool_calling)
            && (!self.code_gen || other.code_gen)
            && other.min_context_window >= self.min_context_window
    }
}

/// Where a model physically runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelSource {
    OnDevice,
    MacLocal,
    MacRemote,
    Hosted { provider: String },
}

/// Measured performance profile for a model.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PerfProfile {
    pub ttft_ms: u32,
    pub tok_per_sec: f32,
}

impl Default for PerfProfile {
    fn default() -> Self {
        Self { ttft_ms: 0, tok_per_sec: 0.0 }
    }
}

/// A user-facing honesty label about a model's limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LimitationLabel {
    NoLocalRepo,
    MaxImages(u8),
    MaxContextWindow(u32),
    NoComplexCodeGen,
    Custom(String),
}

/// A description of one model the system can route to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelDescriptor {
    pub id: String,
    pub display_name: String,
    pub context_window: u32,
    pub capabilities: CapabilitySet,
    pub limitations: Vec<LimitationLabel>,
    pub size_bytes: Option<u64>,
    pub quantization: Option<String>,
    pub source: ModelSource,
    pub perf_profile: PerfProfile,
}

/// The capabilities a single device publishes to the capability registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCapabilitySnapshot {
    pub device_id: Uuid,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub modes: HashMap<ComputeMode, ModeCapability>,
}

/// Per-mode capability detail within a [`DeviceCapabilitySnapshot`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeCapability {
    pub available: bool,
    pub models: Vec<ModelDescriptor>,
    pub tool_ids: Vec<String>,
}
