//! # mahi-ffi
//!
//! UniFFI bindings exposing the Mahi core to the Swift macOS/iOS app. The
//! generated Swift module (`Mahi`) is produced from this crate by
//! `scripts/build-xcframework.sh`.
//!
//! [`MahiEngineHandle`] is the app's single entry point. It owns the agent
//! engine, an async runtime, the managed local-model runtime + download
//! manager, and a runtime-swappable inference provider so the app can:
//!
//! - download and run **local** GGUF models (mode A, Ollama-style and fully
//!   app-managed via `llama-server`),
//! - point chat at a **hosted** Anthropic / OpenAI-compatible model,
//! - stream turns token-by-token with tool use + approvals ([`TurnHandle`]),
//! - spawn parallel subagents.
//!
//! ## The MacLan execution model
//!
//! On the Mac, the Mac itself is the brain that executes tools, so every turn
//! runs in [`ComputeMode::MacLan`] and the [`SwappableProvider`] stamps that
//! mode on every chunk regardless of which model answers. That keeps the full
//! local toolset (files, shell, web, computer use, MCP) available no matter
//! whether a local or hosted model is active — the model's *identity* is shown
//! separately via [`MahiEngineHandle::runtime_status`].

use async_trait::async_trait;
use futures::StreamExt;
use mahi_agent_core::{EngineConfig, MahiEngine};
use mahi_compute::local::catalog::find_entry;
use mahi_compute::local::{DownloadHandle, DownloadStatus, RunningServer};
use mahi_compute::{
    model_catalog, AnthropicProvider, LlamaRuntime, LocalLlamaProvider, ModelManager,
    OpenAiCompatProvider,
};
use mahi_contracts::agent::AgentEvent;
use mahi_contracts::compute::{
    CanHandleResult, FinishReason, InferenceChunk, InferenceProvider, InferenceRequest,
    InferenceStream,
};
use mahi_contracts::error::ContractError;
use mahi_contracts::tooling::ToolEvent;
use mahi_contracts::types::{
    CapabilitySet, ComputeMode, ModelDescriptor, ModelSource, PerfProfile,
};
use mahi_tooling::{
    ComputerController, MockComputerController, MouseButton, Screenshot, ToolRegistry, UiBounds,
    UiElement,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use tokio::runtime::Runtime;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

uniffi::setup_scaffolding!();

/// The active local model: `(model_id, its provider)`.
type LocalSlot = (String, Arc<dyn InferenceProvider>);

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

// ───────────────────────────── FFI value types ─────────────────────────────

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

/// One persisted message, flattened for the UI transcript.
#[derive(uniffi::Record)]
pub struct MessageSummary {
    pub id: String,
    pub role: String,
    pub text: String,
    pub sequence_num: i64,
}

/// Install/run state of one catalog model on this machine.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum ModelStateFfi {
    NotInstalled,
    Downloading {
        progress: f64,
        bytes_downloaded: u64,
    },
    Installed,
    Active,
}

/// One catalog model plus its current state, for the Models screen.
#[derive(uniffi::Record)]
pub struct CatalogModelFfi {
    pub id: String,
    pub display_name: String,
    pub family: String,
    pub size_bytes: u64,
    pub quantization: String,
    pub context_window: u32,
    pub tool_calling: bool,
    pub description: String,
    pub state: ModelStateFfi,
}

/// State of the managed local runtime / active model.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum RuntimeStatusFfi {
    NoModel,
    PreparingRuntime,
    Starting { model_id: String },
    Running { model_id: String },
    Failed { message: String },
}

/// Hosted-provider configuration sent from Settings.
#[derive(Debug, Clone, uniffi::Record)]
pub struct HostedConfigFfi {
    /// `"anthropic"` or `"openai"`.
    pub provider: String,
    pub api_key: String,
    pub model: String,
    pub base_url: Option<String>,
}

/// One streamed agent event, flattened for the Swift bridge.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum AgentEventFfi {
    TurnStarted {
        conversation_id: String,
        message_id: String,
    },
    TextDelta {
        text: String,
    },
    ToolProgress {
        text: String,
    },
    ToolResult {
        output: String,
    },
    ToolError {
        message: String,
    },
    ApprovalRequired {
        approval_id: String,
        summary: String,
    },
    TurnFinished {
        reason: String,
    },
    Error {
        message: String,
    },
}

/// Map a core [`AgentEvent`] (or stream error) to its flattened FFI form.
/// Returns `None` for events the UI doesn't render (e.g. mode handoffs, which
/// never happen under the forced-MacLan model).
fn map_event(
    item: Result<AgentEvent, mahi_contracts::error::ContractError>,
) -> Option<AgentEventFfi> {
    match item {
        Err(e) => Some(AgentEventFfi::Error {
            message: e.to_string(),
        }),
        Ok(AgentEvent::TurnStarted {
            conversation_id,
            message_id,
            ..
        }) => Some(AgentEventFfi::TurnStarted {
            conversation_id: conversation_id.to_string(),
            message_id: message_id.to_string(),
        }),
        Ok(AgentEvent::TextDelta { text }) => Some(AgentEventFfi::TextDelta { text }),
        Ok(AgentEvent::Tool { event }) => match event {
            ToolEvent::Result { output, .. } => Some(AgentEventFfi::ToolResult {
                output: render_tool_output(&output),
            }),
            ToolEvent::Error { message, .. } => Some(AgentEventFfi::ToolError { message }),
            ToolEvent::Chunk { data } => Some(AgentEventFfi::ToolProgress { text: data }),
            ToolEvent::Citation { url, title, .. } => Some(AgentEventFfi::ToolProgress {
                text: format!(
                    "{}{}",
                    title.map(|t| format!("{t} — ")).unwrap_or_default(),
                    url
                ),
            }),
            ToolEvent::ApprovalRequired { summary, .. } => {
                Some(AgentEventFfi::ToolProgress { text: summary })
            }
            ToolEvent::Cancelled => Some(AgentEventFfi::ToolProgress {
                text: "cancelled".to_string(),
            }),
        },
        Ok(AgentEvent::ApprovalRequired {
            approval_id,
            summary,
        }) => Some(AgentEventFfi::ApprovalRequired {
            approval_id: approval_id.to_string(),
            summary,
        }),
        Ok(AgentEvent::TurnFinished { reason }) => Some(AgentEventFfi::TurnFinished {
            reason: format!("{reason:?}"),
        }),
        Ok(AgentEvent::Error { message }) => Some(AgentEventFfi::Error { message }),
        Ok(AgentEvent::ModeHandoff { .. }) => None,
    }
}

/// The MCP config file to auto-load, if present. Checks, in order: an explicit
/// `MAHI_MCP_CONFIG` override, the macOS Claude Desktop config, and `~/.claude`.
/// All use the same `{ "mcpServers": { ... } }` shape.
fn default_mcp_config_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("MAHI_MCP_CONFIG") {
        if !explicit.trim().is_empty() {
            return Some(PathBuf::from(explicit));
        }
    }
    let home = std::env::var("HOME").ok()?;
    let candidates = [
        format!("{home}/Library/Application Support/Claude/claude_desktop_config.json"),
        format!("{home}/.claude/claude.json"),
        format!("{home}/.config/mahi/mcp_servers.json"),
    ];
    candidates
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

fn render_tool_output(output: &serde_json::Value) -> String {
    match output {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) => {
            // Common tool result shapes carry their payload under "content".
            if let Some(serde_json::Value::String(s)) = map.get("content") {
                s.clone()
            } else {
                output.to_string()
            }
        }
        other => other.to_string(),
    }
}

// ──────────────────────── Inference provider plumbing ───────────────────────

/// The engine's inference provider: forwards to whichever real provider is
/// active (local / hosted / placeholder) and stamps every chunk with
/// `forced_mode` so the agent loop's tool gating stays stable across models.
struct SwappableProvider {
    forced_mode: ComputeMode,
    current: RwLock<Arc<dyn InferenceProvider>>,
}

impl SwappableProvider {
    fn new(forced_mode: ComputeMode, initial: Arc<dyn InferenceProvider>) -> Self {
        Self {
            forced_mode,
            current: RwLock::new(initial),
        }
    }

    fn swap(&self, provider: Arc<dyn InferenceProvider>) {
        *self.current.write().expect("provider lock poisoned") = provider;
    }

    fn current(&self) -> Arc<dyn InferenceProvider> {
        self.current.read().expect("provider lock poisoned").clone()
    }
}

#[async_trait]
impl InferenceProvider for SwappableProvider {
    fn descriptor(&self) -> ModelDescriptor {
        self.current().descriptor()
    }

    async fn can_handle(&self, req: &InferenceRequest) -> CanHandleResult {
        self.current().can_handle(req).await
    }

    async fn generate(
        &self,
        req: InferenceRequest,
        cancel: CancellationToken,
    ) -> Result<InferenceStream, mahi_contracts::error::ContractError> {
        let inner = self.current(); // clone Arc, release the lock before awaiting
        let stream = inner.generate(req, cancel).await?;
        let mode = self.forced_mode;
        Ok(Box::pin(stream.map(move |item| {
            item.map(|mut chunk| {
                chunk.active_mode = mode;
                chunk
            })
        })))
    }
}

/// The provider used before any model is configured: streams a friendly
/// "set up a model" message instead of failing.
struct PlaceholderProvider;

#[async_trait]
impl InferenceProvider for PlaceholderProvider {
    fn descriptor(&self) -> ModelDescriptor {
        ModelDescriptor {
            id: "none".to_string(),
            display_name: "No model".to_string(),
            context_window: 0,
            capabilities: CapabilitySet::none(),
            limitations: Vec::new(),
            size_bytes: None,
            quantization: None,
            source: ModelSource::OnDevice,
            perf_profile: PerfProfile::default(),
        }
    }

    async fn can_handle(&self, _req: &InferenceRequest) -> CanHandleResult {
        CanHandleResult::capable()
    }

    async fn generate(
        &self,
        _req: InferenceRequest,
        _cancel: CancellationToken,
    ) -> Result<InferenceStream, mahi_contracts::error::ContractError> {
        let msg = "No model is loaded yet. Open the Models tab to download and run a \
                   local model, or add a hosted API key in Settings.";
        let chunks = vec![
            Ok(InferenceChunk::text(msg, ComputeMode::MacLan)),
            Ok(InferenceChunk::finish(
                FinishReason::Stop,
                ComputeMode::MacLan,
            )),
        ];
        Ok(Box::pin(futures::stream::iter(chunks)))
    }
}

// ───────────────────────── Computer-use host bridge ─────────────────────────

/// A captured screen frame handed across the FFI from the Swift controller.
#[derive(uniffi::Record)]
pub struct ScreenshotFfi {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// One UI element the Swift Accessibility walk discovered.
#[derive(uniffi::Record)]
pub struct UiElementFfi {
    pub role: String,
    pub label: Option<String>,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub focused: bool,
}

/// The host's computer-use surface, implemented in Swift by
/// `MahiComputerController`. Calls are synchronous across the boundary; the
/// Rust adapter runs them on a blocking thread so the async agent loop is never
/// stalled, and maps each failure to a recoverable tool error.
#[uniffi::export(callback_interface)]
pub trait ComputerUseHost: Send + Sync {
    fn screenshot(&self) -> Result<ScreenshotFfi, MahiError>;
    fn describe_ui(&self) -> Result<Vec<UiElementFfi>, MahiError>;
    fn click(&self, x: i32, y: i32, button: String) -> Result<(), MahiError>;
    fn move_mouse(&self, x: i32, y: i32) -> Result<(), MahiError>;
    fn type_text(&self, text: String) -> Result<(), MahiError>;
    fn scroll(&self, dx: i32, dy: i32) -> Result<(), MahiError>;
    fn key(&self, combo: String) -> Result<(), MahiError>;
    fn is_sensitive_context(&self) -> bool;
}

fn mouse_button_str(button: MouseButton) -> String {
    match button {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
    }
    .to_string()
}

/// Adapts a foreign [`ComputerUseHost`] to the tooling [`ComputerController`],
/// running each blocking host call off the async agent loop.
struct HostComputerController {
    host: Arc<dyn ComputerUseHost>,
}

impl HostComputerController {
    async fn on_blocking<T, F>(&self, f: F) -> Result<T, ContractError>
    where
        F: FnOnce(Arc<dyn ComputerUseHost>) -> Result<T, MahiError> + Send + 'static,
        T: Send + 'static,
    {
        let host = Arc::clone(&self.host);
        tokio::task::spawn_blocking(move || f(host))
            .await
            .map_err(|e| ContractError::other(format!("computer-use task failed: {e}")))?
            .map_err(|e| ContractError::other(e.to_string()))
    }
}

#[async_trait]
impl ComputerController for HostComputerController {
    async fn screenshot(&self) -> Result<Screenshot, ContractError> {
        let shot = self.on_blocking(|h| h.screenshot()).await?;
        Ok(Screenshot {
            bytes: shot.png,
            width: shot.width,
            height: shot.height,
            description: String::new(),
        })
    }

    async fn describe_ui(&self) -> Result<Vec<UiElement>, ContractError> {
        let elements = self.on_blocking(|h| h.describe_ui()).await?;
        Ok(elements
            .into_iter()
            .map(|e| UiElement {
                role: e.role,
                label: e.label,
                bounds: UiBounds {
                    x: e.x,
                    y: e.y,
                    width: e.width,
                    height: e.height,
                },
                focused: e.focused,
            })
            .collect())
    }

    async fn click(&self, x: i32, y: i32, button: MouseButton) -> Result<(), ContractError> {
        let b = mouse_button_str(button);
        self.on_blocking(move |h| h.click(x, y, b)).await
    }

    async fn move_mouse(&self, x: i32, y: i32) -> Result<(), ContractError> {
        self.on_blocking(move |h| h.move_mouse(x, y)).await
    }

    async fn type_text(&self, text: &str) -> Result<(), ContractError> {
        let text = text.to_string();
        self.on_blocking(move |h| h.type_text(text)).await
    }

    async fn scroll(&self, dx: i32, dy: i32) -> Result<(), ContractError> {
        self.on_blocking(move |h| h.scroll(dx, dy)).await
    }

    async fn key(&self, combo: &str) -> Result<(), ContractError> {
        let combo = combo.to_string();
        self.on_blocking(move |h| h.key(combo)).await
    }

    async fn is_sensitive_context(&self) -> bool {
        let host = Arc::clone(&self.host);
        tokio::task::spawn_blocking(move || host.is_sensitive_context())
            .await
            .unwrap_or(false)
    }
}

// ─────────────────────────── Streaming turn handle ──────────────────────────

/// A bounded queue of turn events, drained by [`TurnHandle::poll_batch`].
struct TurnQueue {
    inner: Mutex<TurnQueueInner>,
    notify: Notify,
}

struct TurnQueueInner {
    events: VecDeque<AgentEventFfi>,
    finished: bool,
}

impl TurnQueue {
    fn new() -> Self {
        Self {
            inner: Mutex::new(TurnQueueInner {
                events: VecDeque::new(),
                finished: false,
            }),
            notify: Notify::new(),
        }
    }

    fn push(&self, event: AgentEventFfi) {
        self.inner
            .lock()
            .expect("turn queue poisoned")
            .events
            .push_back(event);
        self.notify.notify_one();
    }

    fn finish(&self) {
        self.inner.lock().expect("turn queue poisoned").finished = true;
        self.notify.notify_one();
    }

    /// Suspend until at least one event is available, then drain up to `max`.
    /// Returns an empty vec exactly when the turn has finished and drained.
    async fn next_batch(&self, max: usize) -> Vec<AgentEventFfi> {
        loop {
            {
                let mut guard = self.inner.lock().expect("turn queue poisoned");
                if !guard.events.is_empty() {
                    let take = guard.events.len().min(max);
                    return guard.events.drain(..take).collect();
                }
                if guard.finished {
                    return Vec::new();
                }
            }
            self.notify.notified().await;
        }
    }
}

/// A handle on one in-flight agent turn (mirror of the Swift
/// `TurnHandleProtocol`): poll batches of events, or cancel.
#[derive(uniffi::Object)]
pub struct TurnHandle {
    queue: Arc<TurnQueue>,
    cancel: CancellationToken,
    rt: tokio::runtime::Handle,
}

#[uniffi::export]
impl TurnHandle {
    /// Block until at least one event is ready and return up to `max_events`.
    /// Returns an empty list exactly once, after the turn's stream has ended.
    pub fn poll_batch(&self, max_events: u32) -> Vec<AgentEventFfi> {
        let max = (max_events.max(1)) as usize;
        self.rt.block_on(self.queue.next_batch(max))
    }

    /// Request cancellation of the underlying turn.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

// ───────────────────────────── Engine handle ────────────────────────────────

/// Mutable runtime state behind the engine (models, downloads, hosted config).
struct EngineState {
    manager: Arc<ModelManager>,
    runtime: Arc<LlamaRuntime>,
    running: Arc<Mutex<Option<RunningServer>>>,
    status: Arc<Mutex<RuntimeStatusFfi>>,
    downloads: Mutex<HashMap<String, DownloadHandle>>,
    hosted: Mutex<Option<HostedConfigFfi>>,
    /// The active local model `(id, provider)`, kept so we can revert to it
    /// when hosted config is cleared. Shared (`Arc`) so the activation task can
    /// publish into it.
    local: Arc<Mutex<Option<LocalSlot>>>,
}

/// The handle the Swift app holds. Owns the engine and its async runtime.
#[derive(uniffi::Object)]
pub struct MahiEngineHandle {
    engine: MahiEngine,
    provider: Arc<SwappableProvider>,
    state: EngineState,
    rt: Runtime,
}

impl MahiEngineHandle {
    fn build(
        data_dir: Option<String>,
        controller: Arc<dyn ComputerController>,
    ) -> Result<Arc<Self>, MahiError> {
        let rt = Runtime::new().map_err(MahiError::from)?;
        let (data, base_dir) = match data_dir {
            Some(dir) => {
                let path = PathBuf::from(&dir);
                (
                    mahi_data::open_store(&path).map_err(MahiError::from)?,
                    Some(path),
                )
            }
            None => (mahi_data::open_in_memory().map_err(MahiError::from)?, None),
        };

        // Models + the llama runtime live under the data dir (or a temp dir for
        // the in-memory/preview engine).
        let base_dir = base_dir.unwrap_or_else(std::env::temp_dir);
        let manager = Arc::new(ModelManager::new(base_dir.join("models")));
        let runtime = Arc::new(LlamaRuntime::new(base_dir.join("runtime")));

        let provider = Arc::new(SwappableProvider::new(
            ComputeMode::MacLan,
            Arc::new(PlaceholderProvider),
        ));
        let inference: Arc<dyn InferenceProvider> = provider.clone();

        // Scope file/shell tools to the user's home so the coding agent can
        // work across the user's files (writes/shell are approval-gated).
        let workspace = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| base_dir.clone());
        let tools = Arc::new(ToolRegistry::with_builtins_scoped(controller, workspace));

        // Auto-load Claude-Desktop-style MCP servers so their tools are
        // available out of the box, exactly like Claude Desktop. Best-effort:
        // a missing config or a server that fails to start is logged, not fatal.
        if let Some(cfg_path) = default_mcp_config_path() {
            if cfg_path.is_file() {
                let _ = rt.block_on(tools.mount_mcp_servers_from_config(&cfg_path));
            }
        }

        let engine = MahiEngine::new(EngineConfig {
            data,
            inference,
            tools,
            device_id: Uuid::new_v4(),
        });

        Ok(Arc::new(Self {
            engine,
            provider,
            state: EngineState {
                manager,
                runtime,
                running: Arc::new(Mutex::new(None)),
                status: Arc::new(Mutex::new(RuntimeStatusFfi::NoModel)),
                downloads: Mutex::new(HashMap::new()),
                hosted: Mutex::new(None),
                local: Arc::new(Mutex::new(None)),
            },
            rt,
        }))
    }

    fn set_status(&self, status: RuntimeStatusFfi) {
        *self.state.status.lock().expect("status lock poisoned") = status;
    }
}

#[uniffi::export]
impl MahiEngineHandle {
    /// An ephemeral in-memory engine (preview/testing); computer-use tools are
    /// mocked.
    #[uniffi::constructor]
    pub fn in_memory() -> Result<Arc<Self>, MahiError> {
        Self::build(None, Arc::new(MockComputerController::new()))
    }

    /// An engine persisting to the encrypted store under `data_dir`; models and
    /// the local runtime are stored alongside it. Computer-use tools are mocked
    /// (use [`Self::with_store_and_computer`] to drive the real Mac).
    #[uniffi::constructor]
    pub fn with_store(data_dir: String) -> Result<Arc<Self>, MahiError> {
        Self::build(Some(data_dir), Arc::new(MockComputerController::new()))
    }

    /// Like [`Self::with_store`], but the agent's computer-use tools drive the
    /// real Mac through the Swift `host` (screen capture, Accessibility,
    /// CGEvent input).
    #[uniffi::constructor]
    pub fn with_store_and_computer(
        data_dir: String,
        host: Box<dyn ComputerUseHost>,
    ) -> Result<Arc<Self>, MahiError> {
        let controller = Arc::new(HostComputerController { host: host.into() });
        Self::build(Some(data_dir), controller)
    }

    /// Start a new conversation; returns its id. Runs in MacLan (see module docs).
    pub fn create_conversation(&self) -> Result<String, MahiError> {
        let id = self
            .rt
            .block_on(self.engine.create_conversation(ComputeMode::MacLan))
            .map_err(MahiError::from)?;
        Ok(id.to_string())
    }

    /// Start a streaming turn; poll the returned [`TurnHandle`] for events.
    pub fn start_turn(
        &self,
        conversation_id: String,
        text: String,
    ) -> Result<Arc<TurnHandle>, MahiError> {
        let conv = Uuid::parse_str(&conversation_id).map_err(MahiError::from)?;
        let queue = Arc::new(TurnQueue::new());
        let cancel = CancellationToken::new();

        let engine = self.engine.clone();
        let task_queue = Arc::clone(&queue);
        let task_cancel = cancel.clone();
        self.rt.spawn(async move {
            match engine.run_turn(conv, text, task_cancel).await {
                Ok(mut stream) => {
                    while let Some(item) = stream.next().await {
                        if let Some(event) = map_event(item) {
                            task_queue.push(event);
                        }
                    }
                }
                Err(e) => task_queue.push(AgentEventFfi::Error {
                    message: e.to_string(),
                }),
            }
            task_queue.finish();
        });

        Ok(Arc::new(TurnHandle {
            queue,
            cancel,
            rt: self.rt.handle().clone(),
        }))
    }

    /// Send `text` and return the assistant's full reply (buffered convenience).
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

    /// Respond to a pending approval (id from `AgentEventFfi::ApprovalRequired`).
    pub fn resolve_approval(&self, approval_id: String, approved: bool) -> Result<(), MahiError> {
        let id = Uuid::parse_str(&approval_id).map_err(MahiError::from)?;
        self.rt
            .block_on(self.engine.resolve_approval(id, approved))
            .map_err(MahiError::from)
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

    /// The persisted transcript of a conversation, in order.
    pub fn history(&self, conversation_id: String) -> Result<Vec<MessageSummary>, MahiError> {
        let conv = Uuid::parse_str(&conversation_id).map_err(MahiError::from)?;
        let msgs = self
            .rt
            .block_on(self.engine.history(conv))
            .map_err(MahiError::from)?;
        Ok(msgs
            .into_iter()
            .map(|m| MessageSummary {
                id: m.id.to_string(),
                role: format!("{:?}", m.role).to_lowercase(),
                text: m.text_content(),
                sequence_num: m.sequence_num,
            })
            .collect())
    }

    /// Delegate goals to parallel subagents; returns one summary per goal.
    pub fn spawn_subagents(&self, goals: Vec<String>) -> Result<Vec<String>, MahiError> {
        self.rt
            .block_on(self.engine.spawn_subagents(goals))
            .map_err(MahiError::from)
    }

    // ───────────────────────── Model management ─────────────────────────

    /// The full local-model catalog with each model's current state.
    pub fn model_catalog(&self) -> Result<Vec<CatalogModelFfi>, MahiError> {
        let installed = self.rt.block_on(self.state.manager.installed());
        let installed_ids: HashSet<String> = installed.into_iter().map(|m| m.id).collect();
        let active = self
            .state
            .local
            .lock()
            .expect("local lock poisoned")
            .as_ref()
            .map(|(id, _)| id.clone());
        let downloads = self
            .state
            .downloads
            .lock()
            .expect("downloads lock poisoned");

        let out = model_catalog()
            .into_iter()
            .map(|e| {
                let state = if active.as_deref() == Some(e.id.as_str()) {
                    ModelStateFfi::Active
                } else if let Some(handle) = downloads.get(&e.id) {
                    let dl = handle.state();
                    match dl.status() {
                        DownloadStatus::Downloading | DownloadStatus::Idle => {
                            ModelStateFfi::Downloading {
                                progress: dl.progress() as f64,
                                bytes_downloaded: dl.bytes_downloaded(),
                            }
                        }
                        DownloadStatus::Completed => ModelStateFfi::Installed,
                        DownloadStatus::Failed(_) => ModelStateFfi::NotInstalled,
                    }
                } else if installed_ids.contains(&e.id) {
                    ModelStateFfi::Installed
                } else {
                    ModelStateFfi::NotInstalled
                };
                CatalogModelFfi {
                    id: e.id,
                    display_name: e.display_name,
                    family: e.family,
                    size_bytes: e.size_bytes,
                    quantization: e.quantization,
                    context_window: e.context_window,
                    tool_calling: e.tool_calling,
                    description: e.description,
                    state,
                }
            })
            .collect();
        Ok(out)
    }

    /// Begin downloading a catalog model in the background.
    pub fn start_download(&self, model_id: String) -> Result<(), MahiError> {
        let entry = find_entry(&model_id)
            .ok_or_else(|| MahiError::Engine(format!("unknown model `{model_id}`")))?;
        let handle = self.rt.block_on(self.state.manager.start_download(&entry));
        self.state
            .downloads
            .lock()
            .expect("downloads lock poisoned")
            .insert(model_id, handle);
        Ok(())
    }

    /// Cancel an in-flight download (keeps the partial file for resume).
    pub fn cancel_download(&self, model_id: String) -> Result<(), MahiError> {
        if let Some(handle) = self
            .state
            .downloads
            .lock()
            .expect("downloads lock poisoned")
            .get(&model_id)
        {
            handle.cancel();
        }
        Ok(())
    }

    /// Delete an installed model (and stop it if it's the active one).
    pub fn delete_model(&self, model_id: String) -> Result<(), MahiError> {
        {
            let mut local = self.state.local.lock().expect("local lock poisoned");
            if local.as_ref().map(|(id, _)| id.as_str()) == Some(model_id.as_str()) {
                *local = None;
                *self.state.running.lock().expect("running lock poisoned") = None;
                self.provider.swap(Arc::new(PlaceholderProvider));
                self.set_status(RuntimeStatusFfi::NoModel);
            }
        }
        self.state
            .downloads
            .lock()
            .expect("downloads lock poisoned")
            .remove(&model_id);
        self.rt
            .block_on(self.state.manager.delete(&model_id))
            .map_err(MahiError::from)
    }

    /// Load a downloaded model into the managed runtime with a chosen context
    /// window (in tokens, clamped to the model's max). Non-blocking: poll
    /// [`Self::runtime_status`] for progress, which can take ~a minute. A bigger
    /// context holds more history but uses more memory and runs slower.
    pub fn activate_model(&self, model_id: String, context_tokens: u32) -> Result<(), MahiError> {
        let entry = find_entry(&model_id)
            .ok_or_else(|| MahiError::Engine(format!("unknown model `{model_id}`")))?;
        let path = self.state.manager.model_path(&model_id);
        if !path.is_file() {
            return Err(MahiError::Engine(format!(
                "model `{model_id}` is not downloaded"
            )));
        }
        // Honor the user's choice, clamped to the model's trained max context.
        let ctx = context_tokens.clamp(512, entry.context_window);
        // Keep the agent's history budget in step with the server's window.
        self.engine.set_context_window(ctx as usize);
        self.set_status(RuntimeStatusFfi::Starting {
            model_id: model_id.clone(),
        });

        let runtime = Arc::clone(&self.state.runtime);
        let provider = Arc::clone(&self.provider);
        let running = Arc::clone(&self.state.running);
        let status = Arc::clone(&self.state.status);
        let local = Arc::clone(&self.state.local);
        self.rt.spawn(async move {
            match runtime.start(&path, ctx).await {
                Ok(server) => {
                    let local_provider: Arc<dyn InferenceProvider> =
                        Arc::new(LocalLlamaProvider::new(server.base_url(), &entry));
                    provider.swap(Arc::clone(&local_provider));
                    *running.lock().expect("running lock poisoned") = Some(server);
                    *local.lock().expect("local lock poisoned") =
                        Some((model_id.clone(), local_provider));
                    *status.lock().expect("status lock poisoned") =
                        RuntimeStatusFfi::Running { model_id };
                }
                Err(e) => {
                    *status.lock().expect("status lock poisoned") = RuntimeStatusFfi::Failed {
                        message: e.to_string(),
                    };
                }
            }
        });
        Ok(())
    }

    /// The current runtime/active-model status.
    pub fn runtime_status(&self) -> RuntimeStatusFfi {
        self.state
            .status
            .lock()
            .expect("status lock poisoned")
            .clone()
    }

    /// Set the per-turn context window in tokens — how much history the agent
    /// sees. Larger means more context but slower / more memory; the user can
    /// go up to ~1M for hosted models. Takes effect on the next turn. For local
    /// models the server's window is fixed at activation, so also re-`Run` the
    /// model to change how much it can actually process.
    pub fn set_context_window(&self, context_tokens: u32) {
        self.engine.set_context_window(context_tokens as usize);
    }

    /// Toggle extended thinking and set its token budget for subsequent turns.
    /// When on, capable models (e.g. Anthropic) deliberate within `budget_tokens`
    /// before answering — better on hard, multi-step tasks — and the reasoning
    /// is replayed across tool calls. When off, turns are plain. Local models
    /// without a thinking mode ignore it. Takes effect on the next turn.
    pub fn set_thinking_config(&self, enabled: bool, budget_tokens: u32) {
        self.engine.set_thinking_config(enabled, budget_tokens);
    }

    /// Set (empty clears) a persistent goal for `conversation_id` — an
    /// instruction injected into every turn of that conversation, on top of the
    /// default agent prompt. The seam behind the `/goal` control.
    pub fn set_goal(&self, conversation_id: String, goal: String) -> Result<(), MahiError> {
        let conv = Uuid::parse_str(&conversation_id).map_err(MahiError::from)?;
        let goal = if goal.trim().is_empty() {
            None
        } else {
            Some(goal)
        };
        self.rt
            .block_on(self.engine.set_system_prompt(conv, goal))
            .map_err(MahiError::from)
    }

    /// Compact `conversation_id`: summarize it with the active model and pin the
    /// summary so key context survives the sliding window. The `/compact`
    /// control. Returns the summary (may be slow — one inference call).
    pub fn compact(&self, conversation_id: String) -> Result<String, MahiError> {
        let conv = Uuid::parse_str(&conversation_id).map_err(MahiError::from)?;
        self.rt
            .block_on(self.engine.compact_conversation(conv))
            .map_err(MahiError::from)
    }

    /// Point inference at a hosted provider, or clear it (`None`) to fall back
    /// to the active local model (or the placeholder if none).
    pub fn set_hosted_config(&self, config: Option<HostedConfigFfi>) -> Result<(), MahiError> {
        match config {
            Some(cfg) => {
                let provider: Arc<dyn InferenceProvider> = match cfg.provider.as_str() {
                    "anthropic" => Arc::new(AnthropicProvider::with_model(
                        cfg.api_key.clone(),
                        cfg.model.clone(),
                    )),
                    _ => {
                        let base = cfg
                            .base_url
                            .clone()
                            .unwrap_or_else(|| "https://api.openai.com".to_string());
                        Arc::new(OpenAiCompatProvider::new(
                            base,
                            cfg.api_key.clone(),
                            cfg.model.clone(),
                        ))
                    }
                };
                self.provider.swap(provider);
                self.set_status(RuntimeStatusFfi::Running {
                    model_id: cfg.model.clone(),
                });
                *self.state.hosted.lock().expect("hosted lock poisoned") = Some(cfg);
            }
            None => {
                *self.state.hosted.lock().expect("hosted lock poisoned") = None;
                let local = self
                    .state
                    .local
                    .lock()
                    .expect("local lock poisoned")
                    .clone();
                match local {
                    Some((id, provider)) => {
                        self.provider.swap(provider);
                        self.set_status(RuntimeStatusFfi::Running { model_id: id });
                    }
                    None => {
                        self.provider.swap(Arc::new(PlaceholderProvider));
                        self.set_status(RuntimeStatusFfi::NoModel);
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod computer_bridge_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct FakeHost {
        clicks: AtomicUsize,
        keys: AtomicUsize,
    }

    impl ComputerUseHost for FakeHost {
        fn screenshot(&self) -> Result<ScreenshotFfi, MahiError> {
            Ok(ScreenshotFfi {
                png: vec![1, 2, 3],
                width: 4,
                height: 5,
            })
        }
        fn describe_ui(&self) -> Result<Vec<UiElementFfi>, MahiError> {
            Ok(vec![UiElementFfi {
                role: "button".to_string(),
                label: Some("OK".to_string()),
                x: 1,
                y: 2,
                width: 3,
                height: 4,
                focused: true,
            }])
        }
        fn click(&self, _x: i32, _y: i32, _button: String) -> Result<(), MahiError> {
            self.clicks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn move_mouse(&self, _x: i32, _y: i32) -> Result<(), MahiError> {
            Ok(())
        }
        fn type_text(&self, _text: String) -> Result<(), MahiError> {
            Ok(())
        }
        fn scroll(&self, _dx: i32, _dy: i32) -> Result<(), MahiError> {
            Ok(())
        }
        fn key(&self, _combo: String) -> Result<(), MahiError> {
            self.keys.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn is_sensitive_context(&self) -> bool {
            false
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn host_controller_forwards_to_the_foreign_host() {
        let host = Arc::new(FakeHost::default());
        let controller = HostComputerController { host: host.clone() };

        let shot = controller.screenshot().await.unwrap();
        assert_eq!((shot.width, shot.height), (4, 5));
        assert_eq!(shot.bytes, vec![1, 2, 3]);

        let ui = controller.describe_ui().await.unwrap();
        assert_eq!(ui.len(), 1);
        assert_eq!(ui[0].role, "button");
        assert_eq!(
            ui[0].bounds,
            UiBounds {
                x: 1,
                y: 2,
                width: 3,
                height: 4
            }
        );

        controller.click(10, 20, MouseButton::Left).await.unwrap();
        controller.key("cmd+s").await.unwrap();
        assert_eq!(host.clicks.load(Ordering::SeqCst), 1);
        assert_eq!(host.keys.load(Ordering::SeqCst), 1);
        assert!(!controller.is_sensitive_context().await);
    }
}
