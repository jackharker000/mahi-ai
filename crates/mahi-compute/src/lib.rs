//! mahi-compute — inference router + on-device/hosted providers and mode selection.
//!
//! Implements domain ③ (`docs/backend/domains/03-compute-serving.md`):
//!
//! - [`InferenceRouter`] — implements [`mahi_contracts::compute::InferenceProvider`]
//!   by routing each request to the best available sub-provider, honoring the
//!   local-first preference order (`docs/backend/DECISIONS.md` D1:
//!   MacLan > MacRemote > Hosted, with OnDevice the offline/last-resort default)
//!   and an optional user pin (a pinned-but-unavailable mode is a hard
//!   [`mahi_contracts::error::InferenceError::PinViolation`], never a silent fallback).
//! - [`OnDeviceProvider`] — a dependency-free mode-A provider (canned/echo streaming),
//!   always available offline.
//! - Hosted providers (behind the `hosted` feature): [`hosted::OpenAiCompatProvider`]
//!   and [`hosted::AnthropicProvider`], streaming over SSE.
//! - [`build_capability_snapshot`] — packages registered providers into a
//!   [`mahi_contracts::types::DeviceCapabilitySnapshot`] for the capability registry.

pub mod capability;
pub mod on_device;
pub mod router;

#[cfg(feature = "hosted")]
pub mod hosted;

#[cfg(feature = "local-llm")]
pub mod local;

pub use capability::build_capability_snapshot;
pub use on_device::OnDeviceProvider;
pub use router::{InferenceRouter, InferenceRouterBuilder};

#[cfg(feature = "hosted")]
pub use hosted::{AnthropicProvider, OpenAiCompatProvider};

#[cfg(feature = "local-llm")]
pub use local::{
    catalog_descriptor, model_catalog, CatalogEntry, LlamaRuntime, LocalError, LocalLlamaProvider,
    ModelManager, RuntimeStatus,
};
