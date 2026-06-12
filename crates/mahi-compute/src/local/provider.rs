//! [`LocalLlamaProvider`]: an [`InferenceProvider`] backed by a managed local
//! `llama-server`.
//!
//! It is a thin wrapper over the existing [`OpenAiCompatProvider`] pointed at
//! the loopback server [`crate::local::LlamaRuntime`] runs, with two
//! differences: chunks are stamped [`ComputeMode::OnDevice`] (this is mode A,
//! the on-device/local brain), and [`InferenceProvider::descriptor`] /
//! [`InferenceProvider::can_handle`] report the catalog model's real
//! capabilities (context window, tool calling) so the router routes honestly.

use crate::hosted::OpenAiCompatProvider;
use crate::local::catalog::{catalog_descriptor, CatalogEntry};
use async_trait::async_trait;
use mahi_contracts::compute::{
    CanHandleResult, InferenceProvider, InferenceRequest, InferenceStream,
};
use mahi_contracts::error::ContractError;
use mahi_contracts::types::{ComputeMode, ModelDescriptor};
use tokio_util::sync::CancellationToken;

/// An [`InferenceProvider`] talking to a local `llama-server` over loopback.
pub struct LocalLlamaProvider {
    inner: OpenAiCompatProvider,
    descriptor: ModelDescriptor,
}

impl LocalLlamaProvider {
    /// Build a provider for a running server (`base_url`, e.g.
    /// `http://127.0.0.1:8080`) serving the catalog `entry`.
    pub fn new(base_url: impl Into<String>, entry: &CatalogEntry) -> Self {
        let descriptor = catalog_descriptor(entry);
        Self::with_descriptor(base_url, &entry.id, descriptor)
    }

    /// Build a provider for an arbitrary (possibly non-catalog) model id and a
    /// caller-supplied descriptor.
    pub fn with_descriptor(
        base_url: impl Into<String>,
        model_id: &str,
        descriptor: ModelDescriptor,
    ) -> Self {
        // llama-server ignores the API key; the model field is informational
        // for a single-model server but we pass the real id for clarity.
        let inner = OpenAiCompatProvider::new(base_url, "", model_id.to_string())
            .with_mode(ComputeMode::OnDevice);
        Self { inner, descriptor }
    }

    /// The loopback chat-completions endpoint this provider POSTs to.
    pub fn endpoint(&self) -> String {
        self.inner.chat_completions_url()
    }
}

#[async_trait]
impl InferenceProvider for LocalLlamaProvider {
    fn descriptor(&self) -> ModelDescriptor {
        self.descriptor.clone()
    }

    async fn can_handle(&self, req: &InferenceRequest) -> CanHandleResult {
        let caps = &self.descriptor.capabilities;
        if req.required_caps.satisfied_by(caps) {
            CanHandleResult::capable()
        } else {
            // A local model can't grow a tool-calling head or a bigger context;
            // hint the caller to escalate to a hosted model.
            CanHandleResult {
                capable: false,
                missing_caps: req.required_caps.missing_from(caps),
                escalation_hint: Some(ComputeMode::Hosted),
            }
        }
    }

    async fn generate(
        &self,
        req: InferenceRequest,
        cancel: CancellationToken,
    ) -> Result<InferenceStream, ContractError> {
        self.inner.generate(req, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::catalog::find_entry;
    use mahi_contracts::compute::InferenceRequest;
    use mahi_contracts::types::{CapabilitySet, ModelSource};

    #[test]
    fn descriptor_comes_from_the_catalog_entry() {
        let entry = find_entry("qwen2.5-7b-instruct").unwrap();
        let provider = LocalLlamaProvider::new("http://127.0.0.1:8080", &entry);
        let desc = provider.descriptor();
        assert_eq!(desc.id, "qwen2.5-7b-instruct");
        assert_eq!(desc.source, ModelSource::OnDevice);
        assert!(desc.capabilities.tool_calling);
        assert_eq!(
            provider.endpoint(),
            "http://127.0.0.1:8080/v1/chat/completions"
        );
    }

    #[tokio::test]
    async fn can_handle_is_capable_for_a_plain_request() {
        let entry = find_entry("llama-3.1-8b-instruct").unwrap();
        let provider = LocalLlamaProvider::new("http://127.0.0.1:1", &entry);
        let req = InferenceRequest::from_messages(vec![]);
        assert!(provider.can_handle(&req).await.capable);
    }

    #[tokio::test]
    async fn can_handle_refuses_tool_calling_for_a_non_tool_model() {
        // Gemma 2 has no tool calling; a request that requires it can't be served.
        let entry = find_entry("gemma-2-9b-it").unwrap();
        let provider = LocalLlamaProvider::new("http://127.0.0.1:1", &entry);
        let mut req = InferenceRequest::from_messages(vec![]);
        req.required_caps = CapabilitySet {
            tool_calling: true,
            ..CapabilitySet::none()
        };
        let result = provider.can_handle(&req).await;
        assert!(!result.capable);
        assert!(result.missing_caps.tool_calling);
        assert_eq!(result.escalation_hint, Some(ComputeMode::Hosted));
    }

    #[tokio::test]
    async fn can_handle_refuses_context_beyond_the_model() {
        let entry = find_entry("gemma-2-9b-it").unwrap(); // 8192 ctx
        let provider = LocalLlamaProvider::new("http://127.0.0.1:1", &entry);
        let mut req = InferenceRequest::from_messages(vec![]);
        req.required_caps = CapabilitySet {
            min_context_window: 131_072,
            ..CapabilitySet::none()
        };
        assert!(!provider.can_handle(&req).await.capable);
    }
}
