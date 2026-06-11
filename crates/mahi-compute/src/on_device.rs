//! Mode A: a real, dependency-free on-device provider.
//!
//! [`OnDeviceProvider`] streams a canned (or echoed) reply word-by-word. It is
//! always available, requires no network, and is the offline/privacy default
//! per `docs/backend/DECISIONS.md` D1. On Apple platforms the production
//! runtime (Apple Foundation Models / MLX) is injected from Swift; this type
//! is the portable Rust stand-in that keeps the full pipeline working
//! everywhere (tests, CLI, Linux daemon).

use async_trait::async_trait;
use futures::stream;
use mahi_contracts::compute::{
    CanHandleResult, FinishReason, InferenceChunk, InferenceProvider, InferenceRequest,
    InferenceStream,
};
use mahi_contracts::error::ContractError;
use mahi_contracts::types::{
    CapabilitySet, ComputeMode, LimitationLabel, ModelDescriptor, ModelSource, PerfProfile,
};
use tokio_util::sync::CancellationToken;

/// The default canned reply streamed by [`OnDeviceProvider::new`].
pub const DEFAULT_REPLY: &str = "Hello from Mahi. This is the on-device model.";

/// A first-class mode-A provider: canned/echo streaming, fully offline.
pub struct OnDeviceProvider {
    /// `Some(reply)` streams a fixed reply; `None` echoes the last user message.
    reply: Option<String>,
    descriptor: ModelDescriptor,
}

impl OnDeviceProvider {
    /// Provider streaming the default canned reply.
    pub fn new() -> Self {
        Self::canned(DEFAULT_REPLY)
    }

    /// Provider streaming a fixed canned reply.
    pub fn canned(reply: impl Into<String>) -> Self {
        Self { reply: Some(reply.into()), descriptor: Self::default_descriptor() }
    }

    /// Provider that echoes the user's last message back (`"You said: ..."`).
    pub fn echo() -> Self {
        Self { reply: None, descriptor: Self::default_descriptor() }
    }

    fn default_descriptor() -> ModelDescriptor {
        ModelDescriptor {
            id: "mahi-on-device-v0".to_string(),
            display_name: "Mahi On-Device (mode A)".to_string(),
            context_window: 4096,
            capabilities: CapabilitySet {
                vision: false,
                tool_calling: false,
                // What this model *offers*: callers' `required_caps.min_context_window`
                // is checked against this value via `CapabilitySet::satisfied_by`.
                min_context_window: 4096,
                code_gen: false,
            },
            limitations: vec![
                LimitationLabel::NoLocalRepo,
                LimitationLabel::NoComplexCodeGen,
                LimitationLabel::MaxContextWindow(4096),
            ],
            size_bytes: None,
            quantization: None,
            source: ModelSource::OnDevice,
            perf_profile: PerfProfile { ttft_ms: 25, tok_per_sec: 40.0 },
        }
    }
}

impl Default for OnDeviceProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl InferenceProvider for OnDeviceProvider {
    fn descriptor(&self) -> ModelDescriptor {
        self.descriptor.clone()
    }

    async fn can_handle(&self, req: &InferenceRequest) -> CanHandleResult {
        let offered = &self.descriptor.capabilities;
        if req.required_caps.satisfied_by(offered) {
            CanHandleResult::capable()
        } else {
            CanHandleResult {
                capable: false,
                missing_caps: req.required_caps.missing_from(offered),
                // Local-first: escalate up the chain starting with the paired Mac.
                escalation_hint: Some(ComputeMode::MacLan),
            }
        }
    }

    async fn generate(
        &self,
        req: InferenceRequest,
        cancel: CancellationToken,
    ) -> Result<InferenceStream, ContractError> {
        let reply = match &self.reply {
            Some(r) => r.clone(),
            None => {
                let last = req.messages.last().map(|m| m.text_content()).unwrap_or_default();
                format!("You said: {last}")
            }
        };

        let words: Vec<String> = reply.split_inclusive(' ').map(str::to_string).collect();
        let mode = ComputeMode::OnDevice;

        // Word-by-word stream that honors mid-stream cancellation: when the
        // token fires we emit a terminal `Cancelled` chunk and stop.
        let stream = stream::unfold(
            (words.into_iter(), false),
            move |(mut words, done): (std::vec::IntoIter<String>, bool)| {
                let cancel = cancel.clone();
                async move {
                    if done {
                        return None;
                    }
                    if cancel.is_cancelled() {
                        return Some((
                            Ok(InferenceChunk::finish(FinishReason::Cancelled, mode)),
                            (words, true),
                        ));
                    }
                    match words.next() {
                        Some(w) => Some((Ok(InferenceChunk::text(w, mode)), (words, false))),
                        None => Some((
                            Ok(InferenceChunk::finish(FinishReason::Stop, mode)),
                            (words, true),
                        )),
                    }
                }
            },
        );

        Ok(Box::pin(stream))
    }
}
