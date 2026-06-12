//! Integration tests for the `MahiEngine` facade, built on the contracts
//! testkit (`MockInferenceProvider`, `in_memory_datastore`) plus tiny local
//! stubs for tools and a scripted tool-calling provider.

use async_trait::async_trait;
use futures::StreamExt;
use mahi_agent_core::{EngineConfig, MahiEngine};
use mahi_contracts::agent::AgentEvent;
use mahi_contracts::compute::{
    CanHandleResult, FinishReason, InferenceChunk, InferenceProvider, InferenceRequest,
    InferenceStream, ToolCallDelta,
};
use mahi_contracts::data::{AuditEventType, AuditOutcome, ContentBlock, DataStore, MessageRole};
use mahi_contracts::error::ContractError;
use mahi_contracts::testkit::{in_memory_datastore, MockInferenceProvider};
use mahi_contracts::tooling::{
    DestructiveLevel, ToolCategory, ToolDescriptor, ToolEvent, ToolEventStream, ToolInvocation,
    ToolInvokeContract,
};
use mahi_contracts::types::{
    CapabilitySet, ComputeMode, ModelDescriptor, ModelSource, PerfProfile,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// ─────────────────────────── Stub tooling ───────────────────────────

/// A tiny in-test `ToolInvokeContract`: zero or one "echo" tool that streams a
/// chunk and a result echoing its args.
struct StubTools {
    descriptors: Vec<ToolDescriptor>,
    invocations: AtomicUsize,
}

impl StubTools {
    fn empty() -> Self {
        Self {
            descriptors: Vec::new(),
            invocations: AtomicUsize::new(0),
        }
    }

    fn with_echo(requires_approval: bool) -> Self {
        Self {
            descriptors: vec![ToolDescriptor {
                id: "echo".to_string(),
                display_name: "Echo".to_string(),
                category: ToolCategory::BuiltIn,
                available_in_modes: vec![ComputeMode::OnDevice],
                required_permissions: Vec::new(),
                input_schema: serde_json::json!({ "type": "object" }),
                output_schema: serde_json::json!({ "type": "object" }),
                requires_approval,
                destructive_level: DestructiveLevel::Low,
            }],
            invocations: AtomicUsize::new(0),
        }
    }

    fn invocation_count(&self) -> usize {
        self.invocations.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ToolInvokeContract for StubTools {
    async fn describe(&self, _mode: ComputeMode) -> Vec<ToolDescriptor> {
        self.descriptors.clone()
    }

    async fn invoke(
        &self,
        call: ToolInvocation,
        _cancel: CancellationToken,
    ) -> Result<ToolEventStream, ContractError> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        let events: Vec<Result<ToolEvent, ContractError>> = vec![
            Ok(ToolEvent::Chunk {
                data: "working...".to_string(),
            }),
            Ok(ToolEvent::Result {
                output: serde_json::json!({ "echoed": call.args }),
                truncated: false,
            }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

// ─────────────────────────── Scripted provider ───────────────────────────

/// First generate() returns a streamed tool call for "echo"; every later call
/// returns plain text. Exercises the multi-step loop.
#[derive(Default)]
struct ToolThenTextProvider {
    calls: AtomicUsize,
}

fn scripted_model() -> ModelDescriptor {
    ModelDescriptor {
        id: "scripted-test".to_string(),
        display_name: "Scripted test model".to_string(),
        context_window: 4096,
        capabilities: CapabilitySet {
            tool_calling: true,
            ..CapabilitySet::none()
        },
        limitations: Vec::new(),
        size_bytes: None,
        quantization: None,
        source: ModelSource::OnDevice,
        perf_profile: PerfProfile::default(),
    }
}

#[async_trait]
impl InferenceProvider for ToolThenTextProvider {
    fn descriptor(&self) -> ModelDescriptor {
        scripted_model()
    }

    async fn can_handle(&self, _req: &InferenceRequest) -> CanHandleResult {
        CanHandleResult::capable()
    }

    async fn generate(
        &self,
        _req: InferenceRequest,
        _cancel: CancellationToken,
    ) -> Result<InferenceStream, ContractError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let mode = ComputeMode::OnDevice;
        let chunks: Vec<Result<InferenceChunk, ContractError>> = if n == 0 {
            // Args split over two deltas to exercise accumulation.
            vec![
                Ok(InferenceChunk {
                    delta: None,
                    tool_call_delta: Some(ToolCallDelta {
                        call_id: "c1".to_string(),
                        tool_id: "echo".to_string(),
                        args_delta: "{\"msg\":".to_string(),
                    }),
                    finish_reason: None,
                    active_mode: mode,
                    latency_hint_ms: None,
                }),
                Ok(InferenceChunk {
                    delta: None,
                    tool_call_delta: Some(ToolCallDelta {
                        call_id: "c1".to_string(),
                        tool_id: "echo".to_string(),
                        args_delta: "\"hi\"}".to_string(),
                    }),
                    finish_reason: None,
                    active_mode: mode,
                    latency_hint_ms: None,
                }),
                Ok(InferenceChunk::finish(FinishReason::ToolCall, mode)),
            ]
        } else {
            vec![
                Ok(InferenceChunk::text("The tool said hi.", mode)),
                Ok(InferenceChunk::finish(FinishReason::Stop, mode)),
            ]
        };
        Ok(Box::pin(futures::stream::iter(chunks)))
    }
}

// ─────────────────────────── Helpers ───────────────────────────

fn engine_with(
    data: DataStore,
    inference: Arc<dyn InferenceProvider>,
    tools: Arc<dyn ToolInvokeContract>,
) -> MahiEngine {
    MahiEngine::new(EngineConfig {
        data,
        inference,
        tools,
        device_id: Uuid::new_v4(),
    })
}

/// Drain a turn stream into a Vec of events, stopping after `TurnFinished`.
async fn drain(mut stream: mahi_agent_core::AgentEventStream) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(item) = stream.next().await {
        let event = item.expect("turn stream yielded an error");
        let finished = matches!(event, AgentEvent::TurnFinished { .. });
        events.push(event);
        if finished {
            break;
        }
    }
    events
}

fn collected_text(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

// ─────────────────────────── Tests ───────────────────────────

#[tokio::test]
async fn run_turn_streams_text_and_persists_assistant_message() {
    let data = in_memory_datastore();
    let engine = engine_with(
        data,
        Arc::new(MockInferenceProvider::default()),
        Arc::new(StubTools::empty()),
    );
    let conversation_id = engine
        .create_conversation(ComputeMode::OnDevice)
        .await
        .unwrap();

    let stream = engine
        .run_turn(
            conversation_id,
            "hello there".to_string(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let events = drain(stream).await;

    // Lifecycle: starts with TurnStarted, ends with TurnFinished(Stop).
    assert!(matches!(
        events.first(),
        Some(AgentEvent::TurnStarted { .. })
    ));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnFinished {
            reason: FinishReason::Stop
        })
    ));

    // Text actually streams (mock emits word-by-word, so several deltas).
    let delta_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TextDelta { .. }))
        .count();
    assert!(
        delta_count > 1,
        "expected streaming deltas, got {delta_count}"
    );
    assert_eq!(
        collected_text(&events),
        "Hello from Mahi. This is the on-device mock model."
    );

    // History: user message then assistant message, both persisted.
    let history = engine.history(conversation_id).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].role, MessageRole::User);
    assert_eq!(history[0].text_content(), "hello there");
    assert_eq!(history[1].role, MessageRole::Assistant);
    assert_eq!(
        history[1].text_content(),
        "Hello from Mahi. This is the on-device mock model."
    );
    assert!(history[1].model_id.is_some());
}

#[tokio::test]
async fn tool_call_round_trip_without_approval_completes_loop() {
    let data = in_memory_datastore();
    let provider = Arc::new(ToolThenTextProvider::default());
    let tools = Arc::new(StubTools::with_echo(false));
    let engine = engine_with(data.clone(), provider.clone(), tools.clone());
    let conversation_id = engine
        .create_conversation(ComputeMode::OnDevice)
        .await
        .unwrap();

    let stream = engine
        .run_turn(
            conversation_id,
            "use the echo tool".to_string(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let events = drain(stream).await;

    // No approval should have been requested.
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::ApprovalRequired { .. })));

    // The tool ran exactly once and its events were forwarded.
    assert_eq!(tools.invocation_count(), 1);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Tool {
            event: ToolEvent::Result { .. }
        }
    )));

    // The loop went back to inference after the tool result.
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(collected_text(&events), "The tool said hi.");
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnFinished {
            reason: FinishReason::Stop
        })
    ));

    // History: user → assistant(tool_call) → tool(result) → assistant(text).
    let history = engine.history(conversation_id).await.unwrap();
    let roles: Vec<MessageRole> = history.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::Tool,
            MessageRole::Assistant
        ]
    );
    assert!(history[1]
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolCall { tool_id, .. } if tool_id == "echo")));
    assert!(history[2]
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolResult { call_id, .. } if call_id == "c1")));
    assert_eq!(history[3].text_content(), "The tool said hi.");

    // One audit event was written for the tool call.
    let audit = data.audit.query(Some(conversation_id), 10).await.unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].event_type, AuditEventType::ToolCall);
    assert_eq!(audit[0].outcome, AuditOutcome::Allowed);
    assert_eq!(audit[0].resource_ref.as_deref(), Some("echo"));
}

#[tokio::test]
async fn spawn_subagents_returns_one_summary_per_goal() {
    let engine = engine_with(
        in_memory_datastore(),
        Arc::new(MockInferenceProvider::default()),
        Arc::new(StubTools::empty()),
    );

    let summaries = engine
        .spawn_subagents(vec!["a".to_string(), "b".to_string()])
        .await
        .unwrap();

    assert_eq!(summaries.len(), 2);
    for summary in &summaries {
        assert_eq!(
            summary,
            "Hello from Mahi. This is the on-device mock model."
        );
    }

    // Each subagent ran in its own fresh conversation.
    let conversations = engine.list_conversations(10).await.unwrap();
    assert_eq!(conversations.len(), 2);
}

#[tokio::test]
async fn approval_gate_parks_turn_until_resolved() {
    let data = in_memory_datastore();
    let provider = Arc::new(ToolThenTextProvider::default());
    let tools = Arc::new(StubTools::with_echo(true));
    let engine = engine_with(data.clone(), provider, tools.clone());
    let conversation_id = engine
        .create_conversation(ComputeMode::OnDevice)
        .await
        .unwrap();

    let mut stream = engine
        .run_turn(
            conversation_id,
            "do something risky".to_string(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    // Consume until the approval request arrives.
    let mut events: Vec<AgentEvent> = Vec::new();
    let mut approval_id = None;
    while let Some(item) = stream.next().await {
        let event = item.unwrap();
        if let AgentEvent::ApprovalRequired {
            approval_id: id, ..
        } = &event
        {
            approval_id = Some(*id);
            events.push(event);
            break;
        }
        events.push(event);
    }
    let approval_id = approval_id.expect("expected an ApprovalRequired event");

    // The turn is parked: no further events, and the tool has not run.
    let parked = tokio::time::timeout(Duration::from_millis(100), stream.next()).await;
    assert!(parked.is_err(), "turn should park awaiting approval");
    assert_eq!(tools.invocation_count(), 0);

    // Approve → the turn resumes, the tool runs, and the loop finishes.
    engine.resolve_approval(approval_id, true).await.unwrap();
    while let Some(item) = stream.next().await {
        let event = item.unwrap();
        let finished = matches!(event, AgentEvent::TurnFinished { .. });
        events.push(event);
        if finished {
            break;
        }
    }

    assert_eq!(tools.invocation_count(), 1);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Tool {
            event: ToolEvent::Result { .. }
        }
    )));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnFinished {
            reason: FinishReason::Stop
        })
    ));

    // The audit trail records the allowed, approval-gated call.
    let audit = data.audit.query(Some(conversation_id), 10).await.unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].outcome, AuditOutcome::Allowed);

    // The id is consumed; resolving again errors.
    assert!(engine.resolve_approval(approval_id, true).await.is_err());
}

#[tokio::test]
async fn denied_approval_skips_tool_and_feeds_denial_back() {
    let data = in_memory_datastore();
    let provider = Arc::new(ToolThenTextProvider::default());
    let tools = Arc::new(StubTools::with_echo(true));
    let engine = engine_with(data.clone(), provider, tools.clone());
    let conversation_id = engine
        .create_conversation(ComputeMode::OnDevice)
        .await
        .unwrap();

    let mut stream = engine
        .run_turn(
            conversation_id,
            "do something risky".to_string(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let mut approval_id = None;
    while let Some(item) = stream.next().await {
        if let AgentEvent::ApprovalRequired {
            approval_id: id, ..
        } = item.unwrap()
        {
            approval_id = Some(id);
            break;
        }
    }
    engine
        .resolve_approval(approval_id.unwrap(), false)
        .await
        .unwrap();

    // Drain to the end: the tool never runs, the loop still completes.
    let mut finished_stop = false;
    while let Some(item) = stream.next().await {
        if let AgentEvent::TurnFinished { reason } = item.unwrap() {
            finished_stop = reason == FinishReason::Stop;
            break;
        }
    }
    assert!(finished_stop);
    assert_eq!(tools.invocation_count(), 0);

    // Audit records the denial; the denial result was fed back to the model.
    let audit = data.audit.query(Some(conversation_id), 10).await.unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].outcome, AuditOutcome::Denied);
    let history = engine.history(conversation_id).await.unwrap();
    assert!(history.iter().any(|m| m.role == MessageRole::Tool));
}

#[tokio::test]
async fn cancellation_finishes_turn_with_cancelled() {
    /// A provider that streams forever until cancelled.
    struct SlowProvider;

    #[async_trait]
    impl InferenceProvider for SlowProvider {
        fn descriptor(&self) -> ModelDescriptor {
            scripted_model()
        }
        async fn can_handle(&self, _req: &InferenceRequest) -> CanHandleResult {
            CanHandleResult::capable()
        }
        async fn generate(
            &self,
            _req: InferenceRequest,
            _cancel: CancellationToken,
        ) -> Result<InferenceStream, ContractError> {
            let s = futures::stream::unfold(0u64, |n| async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Some((
                    Ok(InferenceChunk::text("tick ", ComputeMode::OnDevice)),
                    n + 1,
                ))
            });
            Ok(Box::pin(s))
        }
    }

    let engine = engine_with(
        in_memory_datastore(),
        Arc::new(SlowProvider),
        Arc::new(StubTools::empty()),
    );
    let conversation_id = engine
        .create_conversation(ComputeMode::OnDevice)
        .await
        .unwrap();

    let cancel = CancellationToken::new();
    let mut stream = engine
        .run_turn(conversation_id, "never stop".to_string(), cancel.clone())
        .await
        .unwrap();

    // Let a few deltas through, then cancel.
    let mut saw_delta = false;
    let mut reason = None;
    let mut deltas = 0;
    while let Some(item) = stream.next().await {
        match item.unwrap() {
            AgentEvent::TextDelta { .. } => {
                saw_delta = true;
                deltas += 1;
                if deltas == 3 {
                    cancel.cancel();
                }
            }
            AgentEvent::TurnFinished { reason: r } => {
                reason = Some(r);
                break;
            }
            _ => {}
        }
    }

    assert!(saw_delta);
    assert_eq!(reason, Some(FinishReason::Cancelled));
}
