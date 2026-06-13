//! Local model runtime (behind the `local-llm` feature): Ollama-style managed
//! local models on the Mac.
//!
//! The app owns the whole lifecycle, no user-managed daemon required:
//!
//! - [`catalog`] — a curated list of good Q4_K_M GGUF models on Hugging Face
//!   ([`model_catalog`]) plus the mapping into the contracts'
//!   [`mahi_contracts::types::ModelDescriptor`] ([`catalog_descriptor`]).
//! - [`manager`] — [`ModelManager`] downloads `.gguf` files into a models
//!   directory (streamed to `<file>.part`, atomically renamed, HTTP-Range
//!   resume, poll-friendly [`DownloadState`] for the FFI layer).
//! - [`runtime`] — [`LlamaRuntime`] installs a pinned `llama-server` build
//!   from the ggml-org/llama.cpp GitHub releases and runs it as a child
//!   process (`kill_on_drop`), health-checked on a free loopback port.
//! - [`provider`] — [`LocalLlamaProvider`] implements
//!   [`mahi_contracts::compute::InferenceProvider`] by delegating to the
//!   existing OpenAI-compatible SSE client pointed at the local server, with
//!   chunks stamped [`mahi_contracts::types::ComputeMode::OnDevice`].

pub mod catalog;
pub mod manager;
pub mod provider;
pub mod runtime;

pub use catalog::{catalog_descriptor, model_catalog, CatalogEntry};
pub use manager::{DownloadHandle, DownloadState, DownloadStatus, InstalledModel, ModelManager};
pub use provider::LocalLlamaProvider;
pub use runtime::{pick_free_port, LlamaRuntime, RunningServer, RuntimeStatus, LLAMA_CPP_TAG};

/// Errors from the local model runtime (downloads, process lifecycle, files).
#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("http error: {0}")]
    Http(String),
    #[error("model not found: {0}")]
    ModelNotFound(String),
    #[error("runtime error: {0}")]
    Runtime(String),
    #[error("download cancelled")]
    Cancelled,
}
