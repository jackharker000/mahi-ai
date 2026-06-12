//! # mahi-ffi
//!
//! UniFFI bindings exposing the Mahi core to the Swift macOS/iOS app (and,
//! later, Kotlin on Android). The Swift bindings module (`Mahi`) is generated
//! from this crate by `scripts/build-xcframework.sh`.
//!
//! Phase 0 exposes a small, blocking facade — `MahiEngineHandle` — that the
//! app calls from a background task: build an engine, create a conversation,
//! send a message and get the assistant's full reply, and list conversations.
//! Token-level streaming is a follow-up (the engine already streams natively;
//! the FFI will expose it via a polling handle).

use futures::StreamExt;
use mahi_agent_core::{EngineConfig, MahiEngine};
use mahi_compute::{InferenceRouter, OnDeviceProvider};
use mahi_contracts::agent::AgentEvent;
use mahi_contracts::compute::InferenceProvider;
use mahi_contracts::tooling::ToolInvokeContract;
use mahi_contracts::types::ComputeMode;
use mahi_tooling::{MockComputerController, ToolRegistry};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

uniffi::setup_scaffolding!();

/// Errors surfaced across the FFI boundary.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum MahiError {
    #[error("{0}")]
    Engine(String),
}

impl MahiError {
    fn from<E: std::fmt::Display>(e: E) -> Self {
        MahiError::Engine(e.to_string())
    }
}

/// A conversation as shown in the app's sidebar.
#[derive(uniffi::Record)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub mode: String,
}

/// A single assistant message (the buffered result of a turn).
#[derive(uniffi::Record)]
pub struct AssistantReply {
    pub text: String,
}

/// The handle the Swift app holds. Owns the engine and its async runtime.
#[derive(uniffi::Object)]
pub struct MahiEngineHandle {
    engine: MahiEngine,
    rt: Runtime,
}

impl MahiEngineHandle {
    fn build(data_dir: Option<String>) -> Result<Arc<Self>, MahiError> {
        let rt = Runtime::new().map_err(MahiError::from)?;
        let data = match data_dir {
            Some(dir) => mahi_data::open_store(&PathBuf::from(dir)).map_err(MahiError::from)?,
            None => mahi_data::open_in_memory().map_err(MahiError::from)?,
        };
        let inference: Arc<dyn InferenceProvider> = Arc::new(
            InferenceRouter::builder()
                .add_provider(ComputeMode::OnDevice, Arc::new(OnDeviceProvider::new()))
                .build(),
        );
        let tools: Arc<dyn ToolInvokeContract> = Arc::new(ToolRegistry::with_builtins(Arc::new(
            MockComputerController::new(),
        )));
        let engine = MahiEngine::new(EngineConfig {
            data,
            inference,
            tools,
            device_id: Uuid::new_v4(),
        });
        Ok(Arc::new(Self { engine, rt }))
    }
}

#[uniffi::export]
impl MahiEngineHandle {
    /// An ephemeral in-memory engine (on-device model + built-in tools).
    #[uniffi::constructor]
    pub fn in_memory() -> Result<Arc<Self>, MahiError> {
        Self::build(None)
    }

    /// An engine persisting to the encrypted store under `data_dir`.
    #[uniffi::constructor]
    pub fn with_store(data_dir: String) -> Result<Arc<Self>, MahiError> {
        Self::build(Some(data_dir))
    }

    /// Start a new conversation; returns its id.
    pub fn create_conversation(&self) -> Result<String, MahiError> {
        let id = self
            .rt
            .block_on(self.engine.create_conversation(ComputeMode::OnDevice))
            .map_err(MahiError::from)?;
        Ok(id.to_string())
    }

    /// Send `text` in `conversation_id`; returns the assistant's full reply.
    pub fn send(&self, conversation_id: String, text: String) -> Result<AssistantReply, MahiError> {
        let conv = Uuid::parse_str(&conversation_id).map_err(MahiError::from)?;
        let reply = self.rt.block_on(async {
            let mut stream = self
                .engine
                .run_turn(conv, text, CancellationToken::new())
                .await
                .map_err(MahiError::from)?;
            let mut out = String::new();
            while let Some(ev) = stream.next().await {
                if let Ok(AgentEvent::TextDelta { text }) = ev {
                    out.push_str(&text);
                }
            }
            Ok::<String, MahiError>(out)
        })?;
        Ok(AssistantReply { text: reply })
    }

    /// List recent conversations (most-recent first).
    pub fn list_conversations(&self) -> Result<Vec<ConversationSummary>, MahiError> {
        let convs = self
            .rt
            .block_on(self.engine.list_conversations(50))
            .map_err(MahiError::from)?;
        Ok(convs
            .into_iter()
            .map(|c| ConversationSummary {
                id: c.id.to_string(),
                title: c.title.unwrap_or_else(|| "New conversation".to_string()),
                mode: format!("{:?}", c.mode_at_creation),
            })
            .collect())
    }
}
