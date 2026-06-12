//! The [`MahiEngine`] facade and the streaming agent turn loop.
//!
//! Public API shape is normative per `docs/backend/phase-0/03-engine-facade.md`.

use crate::approval::ApprovalRegistry;
use crate::context::{assemble_context, CONTEXT_CHAR_BUDGET};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use futures::StreamExt;
use mahi_contracts::agent::AgentEvent;
use mahi_contracts::compute::{
    FinishReason, InferenceProvider, InferenceRequest, InferenceStream, ToolSpec,
};
use mahi_contracts::data::{
    AuditActor, AuditEvent, AuditEventType, AuditOutcome, ContentBlock, Conversation, DataStore,
    Message, MessageRole,
};
use mahi_contracts::error::{ContractError, StoreError};
use mahi_contracts::tooling::{ToolDescriptor, ToolEvent, ToolInvocation, ToolInvokeContract};
use mahi_contracts::types::{CapabilitySet, ComputeMode};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Maximum number of inference→tool rounds in a single turn (multi-step cap).
const MAX_TOOL_STEPS: usize = 8;
/// How many recent messages to load from the store per round (pre-compaction).
const HISTORY_LIMIT: usize = 200;
/// How many memory records to recall per turn.
const MEMORY_RECALL_LIMIT: usize = 5;
/// Buffered capacity of the turn event channel.
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// The stream of events produced by one agent turn.
pub type AgentEventStream =
    Pin<Box<dyn futures::Stream<Item = Result<AgentEvent, ContractError>> + Send>>;

/// Everything the engine needs injected from the outside world.
#[derive(Clone)]
pub struct EngineConfig {
    pub data: DataStore,
    /// The compute router (itself an `InferenceProvider`) — see mahi-compute.
    pub inference: Arc<dyn InferenceProvider>,
    pub tools: Arc<dyn ToolInvokeContract>,
    pub device_id: Uuid,
}

/// The agent brain: conversation lifecycle, the streaming turn loop, the
/// approval gate, and subagent delegation.
#[derive(Clone)]
pub struct MahiEngine {
    inner: Arc<EngineInner>,
}

struct EngineInner {
    data: DataStore,
    inference: Arc<dyn InferenceProvider>,
    tools: Arc<dyn ToolInvokeContract>,
    device_id: Uuid,
    approvals: ApprovalRegistry,
    /// Per-turn context character budget (~4 chars/token). Runtime-settable so
    /// the user can trade speed for a larger context window (up to ~1M tokens).
    context_budget_chars: AtomicUsize,
}

impl MahiEngine {
    pub fn new(config: EngineConfig) -> Self {
        Self {
            inner: Arc::new(EngineInner {
                data: config.data,
                inference: config.inference,
                tools: config.tools,
                device_id: config.device_id,
                approvals: ApprovalRegistry::default(),
                context_budget_chars: AtomicUsize::new(CONTEXT_CHAR_BUDGET),
            }),
        }
    }

    /// Set the context window the agent assembles per turn, in tokens. Larger
    /// windows hold more history (and cost more/run slower). Clamped to a sane
    /// floor; ~4 chars/token.
    pub fn set_context_window(&self, tokens: usize) {
        let chars = tokens.saturating_mul(4).max(2_000);
        self.inner
            .context_budget_chars
            .store(chars, Ordering::Relaxed);
    }

    /// The current per-turn context budget, in characters.
    pub fn context_budget_chars(&self) -> usize {
        self.inner.context_budget_chars.load(Ordering::Relaxed)
    }

    /// Clone of the injected seams, used to construct isolated sub-engines.
    pub(crate) fn config_clone(&self) -> EngineConfig {
        EngineConfig {
            data: self.inner.data.clone(),
            inference: self.inner.inference.clone(),
            tools: self.inner.tools.clone(),
            device_id: self.inner.device_id,
        }
    }

    /// Create and persist a new empty conversation in `mode`, returning its id.
    pub async fn create_conversation(&self, mode: ComputeMode) -> Result<Uuid, ContractError> {
        let conversation = Conversation::new(mode);
        let id = conversation.id;
        self.inner.data.conversations.upsert(conversation).await?;
        Ok(id)
    }

    /// List the most recently updated conversations.
    pub async fn list_conversations(
        &self,
        limit: usize,
    ) -> Result<Vec<Conversation>, ContractError> {
        self.inner.data.conversations.list(limit).await
    }

    /// Full ordered message history of a conversation.
    pub async fn history(&self, conversation_id: Uuid) -> Result<Vec<Message>, ContractError> {
        self.inner
            .data
            .messages
            .range(conversation_id, usize::MAX)
            .await
    }

    /// Run one agent turn. Streams text/tool/approval/lifecycle events.
    /// Persists the user message and the final assistant message.
    pub async fn run_turn(
        &self,
        conversation_id: Uuid,
        user_text: String,
        cancel: CancellationToken,
    ) -> Result<AgentEventStream, ContractError> {
        let mut conversation = self
            .inner
            .data
            .conversations
            .get(conversation_id)
            .await?
            .ok_or(ContractError::Store(StoreError::NotFound {
                id: conversation_id,
            }))?;
        let mode = conversation.mode_at_creation;

        // Step 1: persist the user message before the loop starts, so a turn
        // that fails to even begin still records what the user said.
        let sequence = self
            .inner
            .data
            .messages
            .next_sequence(conversation_id)
            .await?;
        let user_message = Message::text(
            conversation_id,
            MessageRole::User,
            user_text.clone(),
            mode,
            sequence,
        );
        let user_message_id = user_message.id;
        self.inner.data.messages.append(user_message).await?;

        // Auto-title the conversation from its first user message so the
        // sidebar shows something meaningful instead of "New conversation".
        if conversation.title.is_none() {
            conversation.title = Some(title_from_text(&user_text));
            let _ = self
                .inner
                .data
                .conversations
                .upsert(conversation.clone())
                .await;
        }

        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let runner = TurnRunner {
            inner: self.inner.clone(),
            conversation,
            user_text,
            user_message_id,
            mode,
            trace_id: Uuid::new_v4(),
            cancel,
            tx,
        };
        tokio::spawn(runner.run());

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    /// Respond to a pending approval (id from `AgentEvent::ApprovalRequired`).
    pub async fn resolve_approval(
        &self,
        approval_id: Uuid,
        approved: bool,
    ) -> Result<(), ContractError> {
        self.inner.approvals.resolve(approval_id, approved)
    }

    /// Set (or clear, with `None`) a conversation's system prompt — the seam
    /// behind the `/goal` control: a persistent instruction injected into every
    /// turn of this conversation.
    pub async fn set_system_prompt(
        &self,
        conversation_id: Uuid,
        prompt: Option<String>,
    ) -> Result<(), ContractError> {
        let mut conversation = self
            .inner
            .data
            .conversations
            .get(conversation_id)
            .await?
            .ok_or(ContractError::Store(StoreError::NotFound {
                id: conversation_id,
            }))?;
        conversation.system_prompt = prompt.filter(|p| !p.trim().is_empty());
        self.inner.data.conversations.upsert(conversation).await
    }

    /// Compact a conversation: summarize it with the active model and pin the
    /// summary into the conversation's instructions, so the key facts survive
    /// even as old messages fall out of the sliding context window. The seam
    /// behind the `/compact` control. Returns the summary.
    pub async fn compact_conversation(
        &self,
        conversation_id: Uuid,
    ) -> Result<String, ContractError> {
        let mut conversation = self
            .inner
            .data
            .conversations
            .get(conversation_id)
            .await?
            .ok_or(ContractError::Store(StoreError::NotFound {
                id: conversation_id,
            }))?;
        let history = self.history(conversation_id).await?;
        if history.is_empty() {
            return Ok(String::new());
        }

        let transcript = history
            .iter()
            .map(|m| format!("{:?}: {}", m.role, m.text_content()))
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "Summarize this conversation concisely for your own future reference, \
             preserving key facts, decisions, file paths, and open tasks. Write the \
             summary only.\n\n{transcript}"
        );
        let request = InferenceRequest::from_messages(vec![Message::text(
            conversation_id,
            MessageRole::User,
            prompt,
            conversation.mode_at_creation,
            0,
        )]);
        let mut stream = self
            .inner
            .inference
            .generate(request, CancellationToken::new())
            .await?;
        let mut summary = String::new();
        while let Some(chunk) = stream.next().await {
            if let Some(delta) = chunk?.delta {
                summary.push_str(&delta);
            }
        }
        let summary = summary.trim().to_string();
        if summary.is_empty() {
            return Ok(summary);
        }

        // Pin the summary into the conversation's persistent instructions.
        let pinned = format!("Summary of the conversation so far:\n{summary}");
        conversation.system_prompt = Some(match conversation.system_prompt.take() {
            Some(existing) if !existing.trim().is_empty() => format!("{existing}\n\n{pinned}"),
            _ => pinned,
        });
        self.inner.data.conversations.upsert(conversation).await?;
        Ok(summary)
    }

    /// Delegate a list of goals to parallel, isolated subagents and collect
    /// each one's final assistant text. See [`crate::SubagentCoordinator`].
    ///
    /// Subagents run in [`ComputeMode::MacLan`] so they get the full local
    /// toolset (files, shell, code, computer use, MCP) — capable coworkers, not
    /// chat-only helpers. Use the coordinator directly to pick another mode.
    pub async fn spawn_subagents(&self, goals: Vec<String>) -> Result<Vec<String>, ContractError> {
        crate::subagent::SubagentCoordinator::from_engine(self)
            .run_goals(goals, ComputeMode::MacLan)
            .await
    }
}

// ─────────────────────────── Turn loop ───────────────────────────

/// What one inference round produced.
enum RoundOutcome {
    /// Generation ended for a non-tool reason.
    Finished(FinishReason),
    /// The model requested one or more tool calls.
    ToolCalls(Vec<PendingToolCall>),
    /// The caller's cancellation token fired.
    Cancelled,
}

/// What executing one tool call produced.
enum ToolStepOutcome {
    Completed,
    Cancelled,
}

/// A tool call accumulated from `ToolCallDelta` chunks.
struct PendingToolCall {
    call_id: String,
    tool_id: String,
    args_raw: String,
}

/// Owns one turn: spawned onto a task, pushes `AgentEvent`s into the channel.
struct TurnRunner {
    inner: Arc<EngineInner>,
    conversation: Conversation,
    user_text: String,
    user_message_id: Uuid,
    /// Active compute mode; updated when the provider reports a handoff.
    mode: ComputeMode,
    trace_id: Uuid,
    cancel: CancellationToken,
    tx: mpsc::Sender<Result<AgentEvent, ContractError>>,
}

impl TurnRunner {
    async fn emit(&self, event: AgentEvent) {
        // A dropped receiver just means nobody is listening anymore; the turn
        // still runs to completion so state stays consistent.
        let _ = self.tx.send(Ok(event)).await;
    }

    async fn run(mut self) {
        self.emit(AgentEvent::TurnStarted {
            conversation_id: self.conversation.id,
            message_id: self.user_message_id,
            mode: self.mode,
        })
        .await;

        let reason = match self.drive().await {
            Ok(reason) => reason,
            Err(e) => {
                self.emit(AgentEvent::Error {
                    message: e.to_string(),
                })
                .await;
                FinishReason::Error
            }
        };

        self.touch_conversation().await;
        self.emit(AgentEvent::TurnFinished { reason }).await;
    }

    /// The think→act→observe loop (steps 2–5 of the turn).
    async fn drive(&mut self) -> Result<FinishReason, ContractError> {
        for _step in 0..MAX_TOOL_STEPS {
            // Step 2: assemble context and tool specs.
            let descriptors = self.inner.tools.describe(self.mode).await;
            let messages = assemble_context(
                &self.inner.data,
                &self.conversation,
                &self.user_text,
                HISTORY_LIMIT,
                MEMORY_RECALL_LIMIT,
                self.inner.context_budget_chars.load(Ordering::Relaxed),
            )
            .await?;
            let request = self.build_request(messages, &descriptors);

            // Step 3: stream generation.
            let stream = self
                .inner
                .inference
                .generate(request, self.cancel.clone())
                .await?;
            let mut segment_text = String::new();
            match self.consume_inference(stream, &mut segment_text).await? {
                RoundOutcome::Cancelled => {
                    self.persist_assistant_text(&segment_text).await?;
                    return Ok(FinishReason::Cancelled);
                }
                RoundOutcome::Finished(reason) => {
                    // Step 5: persist the final assistant message.
                    self.persist_assistant_text(&segment_text).await?;
                    return Ok(reason);
                }
                RoundOutcome::ToolCalls(calls) => {
                    // Step 4: record the assistant tool-call message, then run
                    // each call (gated by approval where required) and loop.
                    let assistant_message_id = self
                        .persist_assistant_tool_calls(&segment_text, &calls)
                        .await?;
                    for call in calls {
                        if self.cancel.is_cancelled() {
                            return Ok(FinishReason::Cancelled);
                        }
                        match self
                            .execute_tool_call(assistant_message_id, &descriptors, call)
                            .await?
                        {
                            ToolStepOutcome::Completed => {}
                            ToolStepOutcome::Cancelled => return Ok(FinishReason::Cancelled),
                        }
                    }
                }
            }
        }

        self.emit(AgentEvent::Error {
            message: format!("tool-step cap ({MAX_TOOL_STEPS}) reached; ending turn"),
        })
        .await;
        Ok(FinishReason::Error)
    }

    fn build_request(
        &self,
        messages: Vec<Message>,
        descriptors: &[ToolDescriptor],
    ) -> InferenceRequest {
        let specs: Vec<ToolSpec> = descriptors
            .iter()
            .map(|d| ToolSpec {
                id: d.id.clone(),
                // TODO(contracts): ToolDescriptor has no long-form description
                // field; display_name is the best available text for now.
                description: d.display_name.clone(),
                input_schema: d.input_schema.clone(),
            })
            .collect();
        InferenceRequest {
            request_id: Uuid::new_v4(),
            messages,
            tools: if specs.is_empty() { None } else { Some(specs) },
            max_tokens: None,
            temperature: None,
            streaming_hint: true,
            // TODO(contracts): derive required_caps from the request (vision
            // blocks, tool presence) once content carries richer media types.
            required_caps: CapabilitySet::none(),
        }
    }

    /// Consume one inference stream: emit `TextDelta`s, accumulate text and
    /// tool-call deltas, honor cancellation, surface mode handoffs.
    async fn consume_inference(
        &mut self,
        mut stream: InferenceStream,
        segment_text: &mut String,
    ) -> Result<RoundOutcome, ContractError> {
        let mut calls: Vec<PendingToolCall> = Vec::new();
        loop {
            let next = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Ok(RoundOutcome::Cancelled),
                chunk = stream.next() => chunk,
            };
            let Some(chunk) = next else {
                // Stream ended without a finish reason; treat as a clean stop.
                return Ok(RoundOutcome::Finished(FinishReason::Stop));
            };
            let chunk = chunk?;

            if chunk.active_mode != self.mode {
                self.emit(AgentEvent::ModeHandoff {
                    from: self.mode,
                    to: chunk.active_mode,
                })
                .await;
                self.mode = chunk.active_mode;
            }

            if let Some(delta) = chunk.delta {
                if !delta.is_empty() {
                    segment_text.push_str(&delta);
                    self.emit(AgentEvent::TextDelta { text: delta }).await;
                }
            }

            if let Some(tc) = chunk.tool_call_delta {
                match calls.iter_mut().find(|c| c.call_id == tc.call_id) {
                    Some(existing) => existing.args_raw.push_str(&tc.args_delta),
                    None => calls.push(PendingToolCall {
                        call_id: tc.call_id,
                        tool_id: tc.tool_id,
                        args_raw: tc.args_delta,
                    }),
                }
            }

            if let Some(reason) = chunk.finish_reason {
                return Ok(match reason {
                    FinishReason::ToolCall if !calls.is_empty() => RoundOutcome::ToolCalls(calls),
                    // A ToolCall finish with no accumulated call is a provider
                    // hiccup; degrade gracefully to a stop.
                    FinishReason::ToolCall => RoundOutcome::Finished(FinishReason::Stop),
                    FinishReason::Cancelled => RoundOutcome::Cancelled,
                    other => RoundOutcome::Finished(other),
                });
            }
        }
    }

    /// Run one tool call: approval gate → invoke → stream events → audit →
    /// append the tool-result message.
    async fn execute_tool_call(
        &mut self,
        assistant_message_id: Uuid,
        descriptors: &[ToolDescriptor],
        call: PendingToolCall,
    ) -> Result<ToolStepOutcome, ContractError> {
        let args = parse_args(&call.args_raw);

        let Some(descriptor) = descriptors.iter().find(|d| d.id == call.tool_id) else {
            let message = format!("model requested unknown tool '{}'", call.tool_id);
            self.emit(AgentEvent::Error {
                message: message.clone(),
            })
            .await;
            self.audit_tool_call(&call, &args, AuditOutcome::Denied, None)
                .await?;
            self.append_tool_result(&call.call_id, serde_json::json!({ "error": message }), None)
                .await?;
            return Ok(ToolStepOutcome::Completed);
        };

        // Approval gate: park until the surface resolves.
        let mut approval_ref: Option<Uuid> = None;
        if descriptor.requires_approval {
            let (approval_id, decision) = self.inner.approvals.register();
            approval_ref = Some(approval_id);
            let summary = format!(
                "Run tool '{}' ({}) with args {}",
                descriptor.display_name,
                descriptor.id,
                truncate_for_summary(&call.args_raw, 200),
            );
            self.emit(AgentEvent::ApprovalRequired {
                approval_id,
                summary,
            })
            .await;

            let approved = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => {
                    self.inner.approvals.discard(approval_id);
                    return Ok(ToolStepOutcome::Cancelled);
                }
                decision = decision => decision.unwrap_or(false),
            };

            if !approved {
                self.audit_tool_call(&call, &args, AuditOutcome::Denied, approval_ref)
                    .await?;
                self.append_tool_result(
                    &call.call_id,
                    serde_json::json!({ "denied": true, "message": "the user denied this action" }),
                    approval_ref,
                )
                .await?;
                return Ok(ToolStepOutcome::Completed);
            }
        }

        let invocation = ToolInvocation {
            invocation_id: Uuid::new_v4(),
            tool_id: call.tool_id.clone(),
            args: args.clone(),
            // TODO(contracts): no distinct session concept yet; the
            // conversation id doubles as the session id in Phase 0.
            session_id: self.conversation.id,
            conversation_id: self.conversation.id,
            message_id: assistant_message_id,
            compute_mode: self.mode,
            trace_id: self.trace_id,
            stream: true,
        };

        let mut output = serde_json::Value::Null;
        match self
            .inner
            .tools
            .invoke(invocation, self.cancel.clone())
            .await
        {
            Ok(mut events) => loop {
                let next = tokio::select! {
                    biased;
                    _ = self.cancel.cancelled() => return Ok(ToolStepOutcome::Cancelled),
                    event = events.next() => event,
                };
                let Some(item) = next else { break };
                match item {
                    Ok(event) => {
                        let tool_cancelled = matches!(event, ToolEvent::Cancelled);
                        match &event {
                            ToolEvent::Result { output: out, .. } => output = out.clone(),
                            ToolEvent::Error { message, .. } => {
                                output = serde_json::json!({ "error": message });
                            }
                            _ => {}
                        }
                        self.emit(AgentEvent::Tool { event }).await;
                        if tool_cancelled {
                            return Ok(ToolStepOutcome::Cancelled);
                        }
                    }
                    Err(e) => {
                        // Tool failures are non-fatal: feed the error back to
                        // the model so it can recover or explain.
                        output = serde_json::json!({ "error": e.to_string() });
                        self.emit(AgentEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                        break;
                    }
                }
            },
            Err(e) => {
                output = serde_json::json!({ "error": e.to_string() });
                self.emit(AgentEvent::Error {
                    message: e.to_string(),
                })
                .await;
            }
        }

        self.audit_tool_call(&call, &args, AuditOutcome::Allowed, approval_ref)
            .await?;
        self.append_tool_result(&call.call_id, output, approval_ref)
            .await?;
        Ok(ToolStepOutcome::Completed)
    }

    /// Persist an assistant text message (skipped when the segment is empty).
    async fn persist_assistant_text(&self, text: &str) -> Result<(), ContractError> {
        if text.is_empty() {
            return Ok(());
        }
        let sequence = self
            .inner
            .data
            .messages
            .next_sequence(self.conversation.id)
            .await?;
        let mut message = Message::text(
            self.conversation.id,
            MessageRole::Assistant,
            text,
            self.mode,
            sequence,
        );
        message.model_id = Some(self.inner.inference.descriptor().id);
        self.inner.data.messages.append(message).await
    }

    /// Persist the assistant message that carries this round's tool calls
    /// (plus any text emitted before them), returning its id.
    async fn persist_assistant_tool_calls(
        &self,
        text: &str,
        calls: &[PendingToolCall],
    ) -> Result<Uuid, ContractError> {
        let sequence = self
            .inner
            .data
            .messages
            .next_sequence(self.conversation.id)
            .await?;
        let mut content: Vec<ContentBlock> = Vec::new();
        if !text.is_empty() {
            content.push(ContentBlock::Text {
                text: text.to_string(),
            });
        }
        for call in calls {
            content.push(ContentBlock::ToolCall {
                call_id: call.call_id.clone(),
                tool_id: call.tool_id.clone(),
                args: parse_args(&call.args_raw),
            });
        }
        let message = Message {
            id: Uuid::new_v4(),
            conversation_id: self.conversation.id,
            role: MessageRole::Assistant,
            content,
            model_id: Some(self.inner.inference.descriptor().id),
            mode: self.mode,
            created_at: chrono::Utc::now(),
            sequence_num: sequence,
            approval_refs: Vec::new(),
        };
        let id = message.id;
        self.inner.data.messages.append(message).await?;
        Ok(id)
    }

    /// Append the tool-result message that feeds the next inference round.
    async fn append_tool_result(
        &self,
        call_id: &str,
        output: serde_json::Value,
        approval_ref: Option<Uuid>,
    ) -> Result<(), ContractError> {
        let sequence = self
            .inner
            .data
            .messages
            .next_sequence(self.conversation.id)
            .await?;

        // A screenshot tool returns the raw image (hex). Always strip those
        // bytes from the textual tool result (never dump them into the prompt),
        // and additionally attach an Image block when the active model can see,
        // so vision models reason about the screen — smart computer use, not
        // blind clicking.
        let mut text_output = output;
        let has_image = text_output
            .get("image_hex")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty());
        let vision = self.inner.inference.descriptor().capabilities.vision;
        let image_block = if has_image && vision {
            screenshot_image_block(&text_output)
        } else {
            None
        };
        if has_image {
            if let Some(obj) = text_output.as_object_mut() {
                obj.remove("image_hex");
                obj.insert(
                    "note".to_string(),
                    serde_json::json!("screenshot captured; image attached for vision models"),
                );
            }
        }

        let mut content = vec![ContentBlock::ToolResult {
            call_id: call_id.to_string(),
            output: text_output,
        }];
        if let Some((media_type, data)) = image_block {
            content.push(ContentBlock::Image { media_type, data });
        }

        let message = Message {
            id: Uuid::new_v4(),
            conversation_id: self.conversation.id,
            role: MessageRole::Tool,
            content,
            model_id: None,
            mode: self.mode,
            created_at: chrono::Utc::now(),
            sequence_num: sequence,
            approval_refs: approval_ref.into_iter().collect(),
        };
        self.inner.data.messages.append(message).await
    }

    /// Write one audit event per tool call (step 4).
    async fn audit_tool_call(
        &self,
        call: &PendingToolCall,
        args: &serde_json::Value,
        outcome: AuditOutcome,
        approval_ref: Option<Uuid>,
    ) -> Result<(), ContractError> {
        let event = AuditEvent {
            event_id: Uuid::new_v4(),
            device_id: self.inner.device_id,
            session_id: Some(self.conversation.id),
            event_type: AuditEventType::ToolCall,
            actor: AuditActor::Agent,
            resource_ref: Some(call.tool_id.clone()),
            outcome,
            metadata: serde_json::json!({
                "call_id": call.call_id,
                "args": args,
                "trace_id": self.trace_id,
                "approval_id": approval_ref,
            }),
            timestamp: chrono::Utc::now(),
            // TODO(contracts): hash-chain linkage is owned by the data layer's
            // AuditLog implementation; the engine cannot know the prior hash.
            prev_hash: String::new(),
        };
        self.inner.data.audit.append(event).await
    }

    /// Best-effort bump of the conversation's `updated_at`.
    async fn touch_conversation(&self) {
        let mut conversation = self.conversation.clone();
        conversation.updated_at = chrono::Utc::now();
        let _ = self.inner.data.conversations.upsert(conversation).await;
    }
}

/// Build an `(media_type, base64)` image from a screenshot tool result's
/// hex-encoded `image_hex` field (PNG bytes), for a vision model's Image block.
fn screenshot_image_block(output: &serde_json::Value) -> Option<(String, String)> {
    let hex = output.get("image_hex")?.as_str()?;
    let bytes = decode_hex(hex)?;
    if bytes.is_empty() {
        return None;
    }
    Some(("image/png".to_string(), BASE64.encode(&bytes)))
}

/// Decode a lowercase/uppercase hex string into bytes (`None` if malformed).
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

/// Derive a short conversation title from the first user message: its first
/// non-empty line, truncated on a character boundary.
fn title_from_text(text: &str) -> String {
    const MAX_CHARS: usize = 48;
    let first_line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("New conversation");
    if first_line.chars().count() <= MAX_CHARS {
        first_line.to_string()
    } else {
        let mut title: String = first_line.chars().take(MAX_CHARS).collect();
        title.push('…');
        title
    }
}

/// Parse accumulated tool-call args; fall back to a JSON string (or `{}` when
/// empty) so a malformed payload still round-trips to the model.
fn parse_args(raw: &str) -> serde_json::Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return serde_json::json!({});
    }
    serde_json::from_str(trimmed).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
}

/// Truncate a string for human-readable approval summaries (UTF-8 safe).
fn truncate_for_summary(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max_chars).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
mod title_tests {
    use super::title_from_text;

    #[test]
    fn uses_first_nonempty_line() {
        assert_eq!(title_from_text("Hello there"), "Hello there");
        assert_eq!(
            title_from_text("  \n  Fix the login bug\nmore"),
            "Fix the login bug"
        );
        assert_eq!(title_from_text("   "), "New conversation");
    }

    #[test]
    fn truncates_long_titles_on_a_char_boundary() {
        let title = title_from_text(&"a".repeat(100));
        assert_eq!(title.chars().count(), 49); // 48 chars + ellipsis
        assert!(title.ends_with('…'));
    }
}
