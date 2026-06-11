# Phase 0 — Workspace Scaffolding & Shared Contract Layer

> Implementation-level plan for the first half of Phase 0 (see [`README.md`](./README.md) for the
> phase synthesis, and [`../DECISIONS.md`](../DECISIONS.md) D0 for the Rust-core decision this
> builds on). Type sketches below are the buildable reference for `mahi-contracts` v0.1.0.

## 1. Cargo workspace layout

```
mahi-ai/
├── Cargo.toml                 ← workspace root
├── rust-toolchain.toml        ← pinned toolchain + targets
├── crates/
│   ├── mahi-contracts/        ← shared cross-domain seam types/traits; zero runtime code, zero I/O
│   ├── mahi-agent-core/       ← ① agent loop, sessions, memory, skills, approval gate
│   ├── mahi-tooling/          ← ② tool registry, executor dispatch, connector broker
│   ├── mahi-compute/          ← ③ inference router, provider impls, capability publisher
│   ├── mahi-connectivity/     ← ④ multiplexed bus, pairing, tunnel, screen stream
│   ├── mahi-data/             ← ⑤ local store, identity, permissions, audit (see 02-trust-backbone.md)
│   └── mahi-ffi/              ← UniFFI bridge; thin Rust shim only → libmahi.xcframework
├── bins/
│   ├── mahi-cli/              ← CLI binary (TTY surface over agent-core)
│   └── mahi-daemon/           ← Mac daemon (launchd; OpenAI-compatible HTTP endpoint)
├── swift/
│   ├── MahiCore/              ← Swift package consuming mahi-ffi; Apple-native shims live here
│   └── generated/             ← UniFFI-generated Swift (committed)
└── .github/workflows/         ← ci.yml + cross-compile.yml
```

**Dependency graph (acyclic, contracts at the bottom):**

```
mahi-contracts
    ↑
    ├── mahi-data   ├── mahi-compute   ├── mahi-connectivity   └── mahi-tooling
              ↑
              └── mahi-agent-core
                        ↑
                        ├── mahi-ffi   ├── mahi-cli (bin)   └── mahi-daemon (bin)
```

Rules: `mahi-agent-core` depends on the four domain crates **only through `mahi-contracts` traits**;
domain crates never import each other; `mahi-ffi` depends only on `mahi-agent-core` + `mahi-data`.

**Workspace root (key excerpts):** edition 2021, `rust-version = "1.82"` (MSRV), workspace deps:
`tokio 1.x` (rt-multi-thread on Mac/Linux, current-thread on iOS via cfg in `mahi-ffi`), `futures`,
`async-trait`, `serde`/`serde_json`, `bytes`, `thiserror 2`, `tracing`, `uuid`, `chrono`,
`uniffi 0.28`, `tokio-util` (CancellationToken).

## 2. Core design choices (justified)

- **Async runtime — Tokio:** the only production-grade runtime compiling unmodified to iOS/macOS/Linux.
- **Streaming idiom — `futures::Stream`:** trait methods return `Pin<Box<dyn Stream<Item = Result<T, ContractError>> + Send>>`; channels are an implementation detail, never the contract boundary.
- **Cancellation — `tokio_util::sync::CancellationToken`** passed into every streaming call; implementors poll `token.cancelled()` in their stream loop.
- **Errors — `thiserror` taxonomy:** top-level `ContractError` with `Inference/Tool/Store/Connectivity/Permission/Cancelled` variants; domain crates `#[from]` their internals.
- **Serialization — serde + JSON** for `MultiplexedEnvelope` payloads in Phase 0; MessagePack stubbed behind a `msgpack` feature for Phase 2 QUIC work.
- **`async-trait`** for `dyn`-dispatched traits (`InferenceProvider`, store family); concrete impls may bypass it with monomorphized `impl Trait` where profiling demands.

## 3. `mahi-contracts` v0.1.0 — type sketch

Modules: `types`, `compute`, `tooling`, `data`, `connectivity`, `error`.

```rust
// types.rs
pub enum ComputeMode { OnDevice, MacLan, MacRemote, Hosted }

pub struct CapabilitySet { pub vision: bool, pub tool_calling: bool,
    pub min_context_window: u32, pub code_gen: bool }

pub struct ModelDescriptor { pub id: String, pub display_name: String,
    pub context_window: u32, pub capabilities: CapabilitySet,
    pub limitations: Vec<LimitationLabel>, pub size_bytes: Option<u64>,
    pub quantization: Option<String>, pub source: ModelSource, pub perf_profile: PerfProfile }
pub enum ModelSource { OnDevice, MacLocal, MacRemote, Hosted { provider: String } }
pub struct PerfProfile { pub ttft_ms: u32, pub tok_per_sec: f32 }
pub enum LimitationLabel { NoLocalRepo, MaxImages(u8), MaxContextWindow(u32),
    NoComplexCodeGen, Custom(String) }

pub struct DeviceCapabilitySnapshot { pub device_id: Uuid,
    pub timestamp: DateTime<Utc>, pub modes: HashMap<ComputeMode, ModeCapability> }
pub struct ModeCapability { pub available: bool, pub models: Vec<ModelDescriptor>,
    pub tool_ids: Vec<String> }
```

```rust
// compute.rs
pub type InferenceStream =
    Pin<Box<dyn Stream<Item = Result<InferenceChunk, ContractError>> + Send>>;

pub struct InferenceRequest { pub request_id: Uuid, pub messages: Vec<Message>,
    pub tools: Option<Vec<ToolSpec>>, pub max_tokens: Option<u32>,
    pub temperature: Option<f32>, pub streaming_hint: bool, pub required_caps: CapabilitySet }
pub struct ToolSpec { pub id: String, pub description: String, pub input_schema: serde_json::Value }

pub struct InferenceChunk { pub delta: Option<String>,
    pub tool_call_delta: Option<ToolCallDelta>, pub finish_reason: Option<FinishReason>,
    pub active_mode: ComputeMode,   // echoed on EVERY chunk
    pub latency_hint_ms: Option<u32> }
pub struct ToolCallDelta { pub call_id: String, pub tool_id: String, pub args_delta: String }
pub enum FinishReason { Stop, ToolCall, MaxTokens, Cancelled, Error }
pub struct CanHandleResult { pub capable: bool, pub missing_caps: CapabilitySet,
    pub escalation_hint: Option<ComputeMode> }

#[async_trait]
pub trait InferenceProvider: Send + Sync {
    fn descriptor(&self) -> ModelDescriptor;
    async fn can_handle(&self, req: &InferenceRequest) -> CanHandleResult;
    async fn generate(&self, req: InferenceRequest, cancel: CancellationToken)
        -> Result<InferenceStream, ContractError>;
}
```

```rust
// tooling.rs
pub struct ToolDescriptor { pub id: String, pub display_name: String,
    pub category: ToolCategory, pub available_in_modes: Vec<ComputeMode>,
    pub required_permissions: Vec<String>, pub input_schema: serde_json::Value,
    pub output_schema: serde_json::Value, pub requires_approval: bool,
    pub destructive_level: DestructiveLevel }
pub enum ToolCategory { BuiltIn, Connector, CodingAgent, ComputerUse }
pub enum DestructiveLevel { Low, High, Critical }

pub struct ToolInvocation { pub invocation_id: Uuid, pub tool_id: String,
    pub args: serde_json::Value, pub session_id: Uuid, pub conversation_id: Uuid,
    pub message_id: Uuid, pub compute_mode: ComputeMode, pub trace_id: Uuid, pub stream: bool }

pub type ToolEventStream = Pin<Box<dyn Stream<Item = Result<ToolEvent, ContractError>> + Send>>;
#[serde(tag = "type")]
pub enum ToolEvent {
    Chunk { data: String },
    Citation { url: String, title: Option<String>, excerpt: Option<String> },
    ApprovalRequired { approval_id: Uuid, summary: String, destructive_level: DestructiveLevel },
    Result { output: serde_json::Value, truncated: bool },
    Error { message: String, retryable: bool },
    Cancelled,
}

#[async_trait]
pub trait ToolInvokeContract: Send + Sync {
    async fn describe(&self, mode: ComputeMode) -> Vec<ToolDescriptor>;
    async fn invoke(&self, call: ToolInvocation, cancel: CancellationToken)
        -> Result<ToolEventStream, ContractError>;
}
```

```rust
// data.rs — records: Conversation, Message (role + ContentBlock: Text/ToolCall/ToolResult/
// ArtifactRef), MemoryRecord (kind, user_visible, user_confirmed), Skill (scope: Global/Space/User),
// TaskItem (status: Pending/Running/AwaitingApproval/Completed/Cancelled/Failed),
// AuditEvent (prev_hash: [u8;32], event_type, actor, outcome — NO delete API),
// PermissionGrant (subject, scope, tier: AllowOnce/AllowAlways/Deny, access: ReadOnly/Click/Full),
// ApprovalRequest/Response (decision: Approve/Deny/AllowAlways).

pub type SubscriptionStream<T> = Pin<Box<dyn Stream<Item = Result<T, ContractError>> + Send>>;

// Trait family (all #[async_trait], Send + Sync):
//   ConversationStore: get/create/update/delete/subscribe(id)
//   MessageStore:      append / range(conv, from, limit) / subscribe_to_conversation
//   MemoryStore:       get/upsert/delete/semantic_search(query, space, limit)/subscribe
//   SkillStore:        get/list_by_scope/upsert/delete
//   TaskStore:         get/upsert/delete/subscribe_all
//   AuditLog:          append/query        // no delete, by design
//   PermissionService: check(subject, scope)/grant/revoke

/// Composite handle passed to domains needing multiple facets.
pub struct DataStore {
    pub conversations: Box<dyn ConversationStore>, pub messages: Box<dyn MessageStore>,
    pub memory: Box<dyn MemoryStore>, pub skills: Box<dyn SkillStore>,
    pub tasks: Box<dyn TaskStore>, pub audit: Box<dyn AuditLog>,
    pub permissions: Box<dyn PermissionService>,
}
```

```rust
// connectivity.rs
#[repr(u8)]
pub enum ChannelId { Control = 0x01, Inference = 0x02, Sync = 0x03, Approval = 0x04,
    StreamVideo = 0x05, Input = 0x06, FileXfr = 0x07, Audio = 0x08 }

pub struct MultiplexedEnvelope { pub channel: ChannelId, pub seq_no: u64,
    pub session_id: Uuid, pub payload_type: String, pub payload_bytes: bytes::Bytes }
```

```rust
// error.rs — ContractError { Inference, Tool, Store, Connectivity, Permission, Cancelled }
// Notable variants: InferenceError::{NoCapableProvider, PinViolation{requested}, ThermalThrottle};
// ToolError::{NotFound, UnavailableInMode, SchemaValidation, SandboxViolation};
// StoreError::{NotFound, Backend, Encryption}; ConnectivityError::{PeerUnreachable, ChannelClosed};
// PermissionError::{Denied, HardRuleBlocked}.
```

## 4. FFI strategy — UniFFI

**Chosen over** cbindgen (hand-written callback adapters, duplicated types) and swift-bridge
(Swift-only, no Kotlin path). UniFFI is production-proven (Firefox iOS/Android), generates Swift
**and Kotlin** from one `.udl`, and supports async.

**Async streaming across the boundary:** `mahi-ffi` wraps `generate()` into an opaque
`StreamingHandle` backed by a `tokio::mpsc` receiver; the `.udl` exposes it as a callback interface
with `poll_batch(max)` (batched to cut crossing overhead) + `cancel()`; a thin Swift extension
adapts it to `AsyncSequence` (`for await chunk in handle.asAsyncSequence()`). `cancel()` must be a
**synchronous** FFI call wired to the `CancellationToken` so it isn't delayed behind the async
dispatch queue.

**Artifacts:** `cargo build --target aarch64-apple-ios/-darwin` + `xcodebuild -create-xcframework`
→ vendored into `swift/MahiCore/Frameworks/`. Android later: same `.udl`, `aarch64-linux-android`
via NDK.

## 5. Repo hygiene

- **MSRV 1.82**, pinned in `rust-toolchain.toml` with the three targets (ios-arm64, darwin-arm64, linux-x86_64).
- **CI (`ci.yml`):** `cargo fmt --check` · `clippy --all-targets -D warnings` · `cargo test --all` · `cargo doc --no-deps` (broken intra-doc links fail).
- **Cross-compile (`cross-compile.yml`):** iOS/macOS/Linux builds on a macOS 14 runner.
- **Lints:** `deny(missing_docs)` in `mahi-contracts` only; `deny(clippy::unwrap_used, clippy::expect_used)` everywhere except tests; `deny(unsafe_code)` everywhere except `mahi-ffi`.
- **Contracts versioning:** starts 0.1.0; minor = additive (default-method or `#[serde(default)]` field, all five dependents updated in the same PR); major = any trait-signature change (single migration PR). `CHANGELOG.md` in the crate tracks every bump.

## 6. Task breakdown (ordered; S ≈ 1d, M ≈ 2–3d, L ≈ 4–5d)

| # | Task | Size | Acceptance criteria |
|---|---|---|---|
| 0.1 | Workspace skeleton (root manifest, 9 stub crates, toolchain file) | S | `cargo build --workspace` and `cargo test --workspace` green on empty code |
| 0.2 | CI pipeline (ci.yml + cross-compile.yml) | S | CI green; iOS cross-compile emits `libmahi_ffi.a` |
| 0.3 | `mahi-contracts` v0.1.0 — all §3 types/traits/errors | M | tests + docs build clean; compiles on all three targets; every public item documented |
| 0.4 | `testkit` mocks (MockInferenceProvider, MockToolInvokeContract, in-memory stores) behind a `testkit` feature | M | streaming round-trip test: 3 chunks collected, each `active_mode == OnDevice` |
| 0.5 | Domain crate stubs (AgentCore, ToolRegistry, InferenceRouter, MultiplexedBusStub, LocalDataStore shells) | M | workspace builds; clippy `-D warnings` passes |
| 0.6 | `mahi-data` MVP — rusqlite + SQLCipher, versioned migrations, all store traits, hash-chained audit | L | temp-dir integration test: 10 audit events appended, queried, chain verifies; green on Linux + macOS |
| 0.7 | `mahi-compute` routing skeleton (first `can_handle`-capable provider wins) | M | router + mock provider complete one streaming generate end-to-end |
| 0.8 | `mahi-agent-core` skeleton loop — `run_turn() → AgentEventStream`; persist assistant message; stub tool-call interception | L | integration test asserts persisted `role: Assistant` message + streamed events |
| 0.9 | `mahi-ffi` UniFFI scaffold + `swift/MahiCore` package + 1 Swift test | L | `swift build` + Swift test pass on macOS; iOS static lib builds |
| 0.10 | **"Hello Agent" e2e test** — LocalDataStore + router + mock provider + run_turn("Hello, Mahi.") | M | ≥1 text chunk, `active_mode == OnDevice`, assistant message persisted; <5 s on clean Linux runner |
| 0.11 | `mahi-daemon` stub — axum `POST /v1/chat/completions` streaming SSE from the router | S | `curl -N` receives ≥3 SSE data lines |

**End state:** compiling workspace; contracts fully documented; each domain crate a structural
stub; working encrypted store; routing + loop skeletons; linkable iOS/macOS lib; one e2e streaming
test; CI green on all targets.

## 7. Risks & open items

- **`async-trait` allocation per call** — fine for `generate()` (per-call), benchmark for hot store paths (`MessageStore::append` per token); bypass with monomorphized impls where it matters.
- **FFI streaming backpressure** — Rust mpsc ↔ Swift async scheduler semantics need explicit testing at ~100 tok/s; `poll_batch` mitigates crossing overhead.
- **Contracts churn cascade** — five dependents; prefer additive changes (`#[serde(default)]`, default methods) until contracts prove stable post-Phase 1.
- **`DataStore` composite vs. test doubles** — provide `DataStore::builder()` in testkit filling unused facets with panicking `UnimplementedStore`.
- **SQLCipher on iOS** — `bundled-sqlcipher`'s OpenSSL dependency can conflict with CommonCrypto; evaluate a CommonCrypto-backed build in Task 0.6 **before** committing.
- **Cancellation across FFI** — validate the synchronous `cancel()` path explicitly in Task 0.9.
- **Connectivity is intentionally minimal** — in-process bus stub only; locking `MultiplexedEnvelope`/`ChannelId` now is what lets Phase 2 slot in QUIC without touching contracts.
