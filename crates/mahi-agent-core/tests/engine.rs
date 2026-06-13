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
                    thinking_delta: None,
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
                    thinking_delta: None,
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

// ─────────────────────────── Recording provider ───────────────────────────

/// A chunk carrying one tool-call delta (args may be split across chunks).
fn tool_call_chunk(call_id: &str, tool_id: &str, args_delta: &str) -> InferenceChunk {
    InferenceChunk {
        delta: None,
        tool_call_delta: Some(ToolCallDelta {
            call_id: call_id.to_string(),
            tool_id: tool_id.to_string(),
            args_delta: args_delta.to_string(),
        }),
        thinking_delta: None,
        finish_reason: None,
        active_mode: ComputeMode::OnDevice,
        latency_hint_ms: None,
    }
}

/// Scripted provider that records every [`InferenceRequest`] it receives.
/// Round `n` replays `scripts[n]` (the last script repeats past the end).
struct RecordingProvider {
    scripts: Vec<Vec<InferenceChunk>>,
    requests: std::sync::Mutex<Vec<InferenceRequest>>,
}

impl RecordingProvider {
    fn new(scripts: Vec<Vec<InferenceChunk>>) -> Self {
        Self {
            scripts,
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<InferenceRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl InferenceProvider for RecordingProvider {
    fn descriptor(&self) -> ModelDescriptor {
        scripted_model()
    }

    async fn can_handle(&self, _req: &InferenceRequest) -> CanHandleResult {
        CanHandleResult::capable()
    }

    async fn generate(
        &self,
        req: InferenceRequest,
        _cancel: CancellationToken,
    ) -> Result<InferenceStream, ContractError> {
        let mut requests = self.requests.lock().unwrap();
        let round = requests.len().min(self.scripts.len() - 1);
        requests.push(req);
        let chunks: Vec<Result<InferenceChunk, ContractError>> =
            self.scripts[round].iter().cloned().map(Ok).collect();
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

/// End-to-end with a REAL builtin tool: round 1 the model calls `file_read`
/// (args split across deltas) against a tempdir-scoped ToolRegistry; round 2
/// it answers using the result. Captured requests prove (a) tool specs are
/// sent on every round and (b) the tool RESULT text reaches round 2.
#[tokio::test]
async fn file_read_round_trip_feeds_result_into_round_two_request() {
    use mahi_tooling::{MockComputerController, ToolRegistry};

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "the secret is kiwi").unwrap();

    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            tool_call_chunk("c1", "file_read", "{\"path\":"),
            tool_call_chunk("c1", "file_read", "\"notes.txt\"}"),
            InferenceChunk::finish(FinishReason::ToolCall, ComputeMode::OnDevice),
        ],
        vec![
            InferenceChunk::text("The note says: the secret is kiwi.", ComputeMode::OnDevice),
            InferenceChunk::finish(FinishReason::Stop, ComputeMode::OnDevice),
        ],
    ]));
    let tools = Arc::new(ToolRegistry::with_builtins_scoped(
        Arc::new(MockComputerController::new()),
        dir.path(),
    ));
    let engine = engine_with(in_memory_datastore(), provider.clone(), tools);
    let conversation_id = engine
        .create_conversation(ComputeMode::OnDevice)
        .await
        .unwrap();

    let stream = engine
        .run_turn(
            conversation_id,
            "what does notes.txt say?".to_string(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let events = drain(stream).await;

    // Event sequence: TurnStarted ... Tool(Result) ... TextDelta ... TurnFinished(Stop).
    assert!(matches!(
        events.first(),
        Some(AgentEvent::TurnStarted { .. })
    ));
    let result_pos = events
        .iter()
        .position(|e| {
            matches!(
                e,
                AgentEvent::Tool {
                    event: ToolEvent::Result { .. }
                }
            )
        })
        .expect("expected a tool Result event");
    let text_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::TextDelta { .. }))
        .expect("expected a text delta after the tool ran");
    assert!(result_pos < text_pos, "tool result must precede final text");
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "no errors expected: {events:?}"
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnFinished {
            reason: FinishReason::Stop
        })
    ));
    assert_eq!(
        collected_text(&events),
        "The note says: the secret is kiwi."
    );

    // Captured requests: two rounds, each carrying the registry's tool specs.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let specs = request.tools.as_ref().expect("tools sent every round");
        let file_read = specs
            .iter()
            .find(|s| s.id == "file_read")
            .expect("file_read spec present");
        assert!(file_read.input_schema["properties"]["path"].is_object());
        assert!(!file_read.description.is_empty());
    }

    // Round 2 transcript carries the assistant tool call and the REAL result.
    let round2 = &requests[1].messages;
    let assistant_call = round2
        .iter()
        .find(|m| {
            m.role == MessageRole::Assistant
                && m.content.iter().any(|b| {
                    matches!(
                        b,
                        ContentBlock::ToolCall { call_id, tool_id, args }
                            if call_id == "c1"
                                && tool_id == "file_read"
                                && args["path"] == "notes.txt"
                    )
                })
        })
        .is_some();
    assert!(
        assistant_call,
        "round 2 must replay the assistant tool call"
    );
    let result_content = round2
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .flat_map(|m| m.content.iter())
        .find_map(|b| match b {
            ContentBlock::ToolResult { call_id, output } if call_id == "c1" => Some(output.clone()),
            _ => None,
        })
        .expect("round 2 must carry the tool result for c1");
    assert_eq!(result_content["content"], "the secret is kiwi");
}

/// Two parallel tool calls in one assistant turn, deltas interleaved across
/// distinct call ids: both execute, and both results reach round 2.
#[tokio::test]
async fn parallel_tool_calls_in_one_round_both_execute_and_feed_back() {
    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            // Interleaved fragments for two distinct calls.
            tool_call_chunk("c1", "echo", "{\"msg\":"),
            tool_call_chunk("c2", "echo", "{\"msg\":"),
            tool_call_chunk("c1", "echo", "\"first\"}"),
            tool_call_chunk("c2", "echo", "\"second\"}"),
            InferenceChunk::finish(FinishReason::ToolCall, ComputeMode::OnDevice),
        ],
        vec![
            InferenceChunk::text("Both tools ran.", ComputeMode::OnDevice),
            InferenceChunk::finish(FinishReason::Stop, ComputeMode::OnDevice),
        ],
    ]));
    let tools = Arc::new(StubTools::with_echo(false));
    let engine = engine_with(in_memory_datastore(), provider.clone(), tools.clone());
    let conversation_id = engine
        .create_conversation(ComputeMode::OnDevice)
        .await
        .unwrap();

    let stream = engine
        .run_turn(
            conversation_id,
            "run echo twice".to_string(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let events = drain(stream).await;

    // Both calls executed, two Result events streamed, turn finished cleanly.
    assert_eq!(tools.invocation_count(), 2);
    let result_events = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::Tool {
                    event: ToolEvent::Result { .. }
                }
            )
        })
        .count();
    assert_eq!(result_events, 2);
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnFinished {
            reason: FinishReason::Stop
        })
    ));

    // One assistant message carries BOTH accumulated tool calls.
    let history = engine.history(conversation_id).await.unwrap();
    let assistant = history
        .iter()
        .find(|m| {
            m.role == MessageRole::Assistant
                && m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolCall { .. }))
        })
        .expect("assistant tool-call message persisted");
    let calls: Vec<(&str, &str, &serde_json::Value)> = assistant
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolCall {
                call_id,
                tool_id,
                args,
            } => Some((call_id.as_str(), tool_id.as_str(), args)),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, "c1");
    assert_eq!(calls[0].2["msg"], "first");
    assert_eq!(calls[1].0, "c2");
    assert_eq!(calls[1].2["msg"], "second");

    // Round 2's request contains a tool result for each call id.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let round2_results: Vec<String> = requests[1]
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolResult { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(round2_results, vec!["c1".to_string(), "c2".to_string()]);
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

#[tokio::test]
async fn thinking_toggle_controls_request_thinking() {
    use mahi_tooling::{MockComputerController, ToolRegistry};

    // A provider that just stops; we only care about the captured request.
    fn stop_script() -> Vec<Vec<InferenceChunk>> {
        vec![vec![
            InferenceChunk::text("ok", ComputeMode::OnDevice),
            InferenceChunk::finish(FinishReason::Stop, ComputeMode::OnDevice),
        ]]
    }

    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(RecordingProvider::new(stop_script()));
    let tools = Arc::new(ToolRegistry::with_builtins_scoped(
        Arc::new(MockComputerController::new()),
        dir.path(),
    ));
    let engine = engine_with(in_memory_datastore(), provider.clone(), tools);
    let conv = engine
        .create_conversation(ComputeMode::OnDevice)
        .await
        .unwrap();

    // Default: thinking is on, so the request carries a budget.
    drain(
        engine
            .run_turn(conv, "hi".to_string(), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await;
    let on = &provider.requests()[0];
    assert!(on.thinking.is_some(), "thinking should be on by default");

    // Toggle off: the next turn's request has no thinking config.
    engine.set_thinking_config(false, 0);
    drain(
        engine
            .run_turn(conv, "again".to_string(), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await;
    let off = provider.requests();
    assert!(
        off.last().unwrap().thinking.is_none(),
        "thinking should be off after disabling"
    );

    // Toggle back on with an explicit budget.
    engine.set_thinking_config(true, 8192);
    drain(
        engine
            .run_turn(conv, "more".to_string(), CancellationToken::new())
            .await
            .unwrap(),
    )
    .await;
    let back = provider.requests();
    assert_eq!(
        back.last().unwrap().thinking.map(|t| t.budget_tokens),
        Some(8192)
    );
}
