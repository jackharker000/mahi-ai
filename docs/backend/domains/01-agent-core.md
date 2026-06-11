# Domain ① — Agent Core / Orchestration Engine

> Detailed design for the shared agent "brain." See [`../README.md`](../README.md) for the
> synthesis, cross-domain contracts, and roadmap. Module/file paths below are **illustrative**
> pending the shared-core language decision (README §5).

## 1. Overview & responsibilities

The Agent Core is the runtime brain of Mahi AI: a compute-mode-agnostic library that runs
in-process on Mac/CLI and as a hosted service for phone clients. Contract: accept a user turn,
manage all state and routing, drive tools and subagents to completion, and stream structured
events back to the surface.

- Drive the multi-turn agent loop end-to-end (streaming + cancellation)
- Own conversation/session lifecycle and context windows
- Manage memory (read/write/surface to user)
- Resolve and invoke skills as first-class orchestration units
- Spawn and coordinate subagents with isolated context
- Gate every destructive action through the approval subsystem
- Route inference to the Compute layer with model hints
- Produce a serializable checkpoint at any point so a mid-conversation mode switch loses nothing

## 2. Component breakdown

| Component | Responsibility | Key boundary |
|---|---|---|
| AgentLoop | Drives think→act→observe; owns streaming + cancellation | Calls InferenceGateway; emits EventStream to surface |
| SessionManager | Creates/loads/serializes conversations; attaches space/persona | Reads/writes DataStore |
| ContextAssembler | Builds the context window: history + memory + skills + system prompt | Reads MemoryStore + SkillRegistry |
| MemorySubsystem | Persists, indexes, auto-recalls facts; user-editable view | Writes DataStore; recall API to ContextAssembler |
| SkillRegistry | Loads/validates/scopes/invokes markdown skill playbooks | Reads skills from DataStore; injects into ContextAssembler |
| SubagentCoordinator | Spawns isolated AgentLoop instances; fans out / collects | Forks context; reports TaskResult |
| ToolBridge | Translates tool-call requests into the Tooling contract; streams events back | Implements ToolInvokeContract |
| ApprovalGate | Intercepts `requires_approval` actions; blocks until surface responds | Surfaces ApprovalRequest; receives ApprovalResponse |
| ModelRouter | Selects fast/deep target per request from hints/load/overrides | Sends RoutingDecision to InferenceGateway |
| ArtifactManager | Manages rendered artifact lifecycle (HTML/SVG/widget/chart/doc) | Persists artifact blobs to DataStore |
| TaskScheduler | Stores + fires scheduled/recurring tasks on wall-clock triggers | Reads DataStore; enqueues into AgentLoop |
| CheckpointManager | Serializes full conversation state for mode handoff | Writes/reads Checkpoint blob to DataStore |

## 3. Key interfaces & data models

```
Conversation { id, space_id, created_at, mode_at_creation: ComputeMode,
  system_prompt, persona_id?, messages: Message[], active_checkpoint_id? }

Message { id, role: user|assistant|tool|system,
  content: ContentBlock[]            // text | tool_call | tool_result | artifact_ref
  model_id?, mode: ComputeMode, created_at, approval_refs: UUID[] }

// Tool contract expected FROM the Tooling layer
ToolInvokeContract {
  list_tools(scope): ToolDescriptor[]
  invoke(call: ToolCall): AsyncIterable<ToolEvent>
  ToolDescriptor { id, name, description, input_schema, requires_approval, destructive_level: low|high|critical }
  ToolCall { id, tool_id, input: JSON, conversation_id, message_id }
  ToolEvent = ToolStreamChunk | ToolResult | ToolError | ToolApprovalRequired
}

MemoryRecord { id, space_id|global, kind: fact|preference|project_note|system,
  content, embedding_ref?, source_message_id?, created_at, updated_at,
  user_visible, user_confirmed }

Skill { id, slug, scope: global|space|user, source_markdown,
  parsed: SkillManifest /* triggers, required_tools, prompt_injection, steps */,
  installed_by, version }

SubagentTask { id, parent_conversation_id, goal, forked_context: ContextSnapshot,
  status: pending|running|complete|failed|cancelled, result?, spawned_at }

ApprovalRequest { id, conversation_id, message_id, tool_call_id, action_summary,
  destructive_level, scope: allow_once|allow_always|deny, expires_at?,
  status: pending|approved|denied|timed_out }

RoutingDecision { request_id, selected_mode: ComputeMode, model_hint: fast|deep|code|vision,
  user_override?, fallback_chain: ComputeMode[], rationale }

Checkpoint { id, conversation_id, captured_at, mode_at_capture,
  messages_snapshot, context_window_state, pending_approvals, active_subagents, memory_recall_ids }

// Expected FROM the Compute layer
InferenceGateway {
  stream_completion(req): AsyncIterable<TokenChunk | ToolCallChunk | DoneEvent>
  get_capabilities(mode): ModelCapabilities  // { max_context, supports_tools, supports_vision, supports_streaming, available_models }
}

// Expected FROM the Data layer
DataStore {
  conversations: CRUD; messages: append + range_query
  memory: CRUD + semantic_search(query, space_id)
  skills: CRUD + list_by_scope; checkpoints: write + latest(conv_id)
  artifacts: write + read; tasks: CRUD; approval_log: append + query
}
```

## 4. Mode-matrix behavior

| Concern | A (on-device) | B (Mac LAN) | C (Mac tunnel) | D (Cloud) |
|---|---|---|---|---|
| AgentLoop runs | On iPhone, in-process | On Mac, served | On Mac, via tunnel | Hosted |
| ContextAssembler | Truncated (small-model limit) | Full | Full | Full |
| ToolBridge | Restricted local tools | Full Mac tools | Full Mac tools | Cloud-safe tools |
| SubagentCoordinator | Disabled / single-level | Full parallel | Full parallel | Full parallel |
| Streaming transport | Local IPC | LAN socket | Encrypted tunnel | HTTPS/SSE |
| ApprovalGate surfaces | Phone | Mac + phone | Mac + phone | Phone |

**Mode switch mid-conversation:** CheckpointManager serializes state → written to DataStore and
propagated (E2E encrypted) → new runtime reconstructs ContextAssembler, re-attaches pending
approvals, resumes subagents → surface receives a `ModeHandoffEvent`. Tool calls that can't migrate
surface a `ToolUnavailableEvent` (user re-runs or skips).

## 5. Deferred decisions (in order)
1. **Agent-loop tool-call representation** — provider-neutral internal `ToolCallChunk` + thin adapters (avoids coupling to one model vendor).
2. **Phone↔Mac streaming transport** — WebSocket-style bidirectional envelope so approval responses flow up the same channel (reconciled in README §4 to the Connectivity `MultiplexedBus`).
3. **Context compaction** — hierarchical summarization (Mac/cloud) + sliding window (on-device).
4. **Memory embedding / vector index** — on-device vs server vs hybrid (cache top-K locally).
5. **Skill format** — Markdown + YAML frontmatter, JSON-schema validated on load.
6. **Subagent isolation** — in-process + budget/timeout first; OS-process for high-risk later.

## 6. Top risks
- Context starvation on mode A (aggressive compaction + UX for "forgetting").
- **Approval-gate deadlock** if the approver device is offline → need timeout/park policy.
- Subagent context/token explosion → depth limit + budget controller.
- Checkpoint migration fidelity mid-stream → restart-safe tool contract required.
- Skill trust (user skills inject system-prompt content) → signing/sandbox before sharing.
- Memory over-collection → the `user_visible`/`user_confirmed` flags plus a conservative extraction heuristic.

## 7. Phased build
- **MVP:** single-turn AgentLoop, SessionManager + Conversation/Message, sliding-window ContextAssembler, static ModelRouter, InferenceGateway stub (one cloud model / mode D), DataStore via SQLite.
- **v1:** streaming loop, ToolBridge + ApprovalGate, MemorySubsystem, SkillRegistry, CheckpointManager + handoff (B→D, A→D), single-level subagents, ArtifactManager, cancellation throughout.
- **Later:** hierarchical summarization, semantic recall, nested/parallel subagents + budgets, TaskScheduler, skill signing, model-override UI, full mode-A on-device loop.

### Illustrative module layout
`agent-core/{types/core, contracts/inference-gateway, contracts/tool-bridge, contracts/data-store, agent-loop}`
