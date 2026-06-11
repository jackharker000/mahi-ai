//! Mode D: hosted provider gateway (behind the `hosted` feature).
//!
//! Two wire formats per domain ③ §5: the OpenAI-compatible chat-completions
//! format as canonical ([`OpenAiCompatProvider`] — also speaks to an Ollama
//! Mac daemon), and a thin Anthropic Messages API adapter
//! ([`AnthropicProvider`]). Both stream over SSE and convert transport/HTTP
//! failures to [`mahi_contracts::error::InferenceError::Provider`].

pub mod anthropic;
pub mod openai;
pub(crate) mod sse;

pub use anthropic::AnthropicProvider;
pub use openai::OpenAiCompatProvider;

use mahi_contracts::error::{ContractError, InferenceError};

/// Map a reqwest transport failure to `InferenceError::Provider`.
pub(crate) fn transport_error(err: reqwest::Error) -> ContractError {
    InferenceError::Provider {
        message: format!("transport error: {err}"),
        retryable: err.is_timeout() || err.is_connect(),
    }
    .into()
}

/// Map a non-success HTTP status (+ response body) to `InferenceError::Provider`.
pub(crate) fn http_error(
    provider: &str,
    status: reqwest::StatusCode,
    body: String,
) -> ContractError {
    InferenceError::Provider {
        message: format!("{provider} HTTP {status}: {body}"),
        retryable: status.is_server_error() || status.as_u16() == 429,
    }
    .into()
}
