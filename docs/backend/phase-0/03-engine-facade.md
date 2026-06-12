# Engine & FFI Facade — the integration contract

> **This file is the single source of truth** that every integrator (FFI, CLI, daemon, Swift app)
> builds against, so the crate agents can work in parallel without API drift. Names here are
> normative.

## `mahi-agent-core` public API

```rust
use mahi_contracts::compute::InferenceProvider;
use mahi_contracts::tooling::ToolInvokeContract;
use mahi_contracts::data::{DataStore, Conversation, Message};
use mahi_contracts::agent::AgentEvent;
use mahi_contracts::types::ComputeMode;
use mahi_contracts::ContractError;
use tokio_util::sync::CancellationToken;
use std::sync::Arc;
use std::pin::Pin;
use futures::Stream;
use uuid::Uuid;

pub type AgentEventStream =
    Pin<Box<dyn Stream<Item = Result<AgentEvent, ContractError>> + Send>>;

pub struct EngineConfig {
    pub data: DataStore,
    /// The compute router (itself an InferenceProvider) — see mahi-compute.
    pub inference: Arc<dyn InferenceProvider>,
    pub tools: Arc<dyn ToolInvokeContract>,
    pub device_id: Uuid,
}

pub struct MahiEngine { /* ... */ }

impl MahiEngine {
    pub fn new(config: EngineConfig) -> Self;

    pub async fn create_conversation(&self, mode: ComputeMode)
        -> Result<Uuid, ContractError>;
    pub async fn list_conversations(&self, limit: usize)
        -> Result<Vec<Conversation>, ContractError>;
    pub async fn history(&self, conversation_id: Uuid)
        -> Result<Vec<Message>, ContractError>;

    /// Run one agent turn. Streams text/tool/approval/lifecycle events.
    /// Persists the user message and the final assistant message.
    pub async fn run_turn(
        &self,
        conversation_id: Uuid,
        user_text: String,
        cancel: CancellationToken,
    ) -> Result<AgentEventStream, ContractError>;

    /// Respond to a pending approval (id from AgentEvent::ApprovalRequired).
    pub async fn resolve_approval(&self, approval_id: Uuid, approved: bool)
        -> Result<(), ContractError>;
}
```

## `mahi-compute` router

```rust
/// Implements InferenceProvider by routing to the best available sub-provider,
/// honoring the local-first preference order (DECISIONS.md D1) and an optional pin.
pub struct InferenceRouter { /* ... */ }
impl InferenceRouter {
    pub fn builder() -> InferenceRouterBuilder;
}
pub struct InferenceRouterBuilder { /* ... */ }
impl InferenceRouterBuilder {
    pub fn add_provider(self, mode: ComputeMode, provider: Arc<dyn InferenceProvider>) -> Self;
    pub fn pin(self, mode: Option<ComputeMode>) -> Self;
    pub fn build(self) -> InferenceRouter;
}
// InferenceRouter: InferenceProvider  (so it drops straight into EngineConfig.inference)
```

## `mahi-data` constructor

```rust
/// Open (or create) the encrypted local store at `path`, returning a wired DataStore.
pub fn open_store(path: &std::path::Path) -> Result<DataStore, ContractError>;
/// In-memory variant for tests/ephemeral use.
pub fn open_in_memory() -> Result<DataStore, ContractError>;
```

## `mahi-tooling` registry

```rust
/// The default tool registry implementing ToolInvokeContract.
pub struct ToolRegistry { /* ... */ }
impl ToolRegistry {
    /// Registry with the standard built-in tools wired to `controller` for computer-use.
    pub fn with_builtins(controller: Arc<dyn ComputerController>) -> Self;
    pub fn empty() -> Self;
}
// ToolRegistry: ToolInvokeContract

/// Platform hook the macOS app implements (screenshot, click, type, ...).
/// A Linux mock impl ships for tests.
pub trait ComputerController: Send + Sync { /* see 02-tooling spec in prompt */ }
```

## FFI surface (`mahi-ffi`, UniFFI)

Exposed to Swift as `MahiEngine` with async methods mirroring the core facade, plus a
`MahiEngineBuilder` that assembles store + router + tools. Streaming `run_turn` is exposed as a
`TurnHandle` with `poll_batch(max) -> [AgentEventFfi]` + `cancel()`, adapted to Swift
`AsyncSequence`. The macOS app injects a real `ComputerController` and a real on-device/hosted
provider through builder callbacks.
