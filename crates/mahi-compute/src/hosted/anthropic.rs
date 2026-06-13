//! Anthropic Messages API adapter (thin adapter per domain ③ §5).
//!
//! POSTs `https://api.anthropic.com/v1/messages` with `stream: true` and
//! parses the SSE event stream: `content_block_delta`/`text_delta` events
//! become text chunks, `input_json_delta` events become tool-call deltas
//! (basic `tool_use` passthrough), `message_delta` records the stop reason,
//! and `message_stop` terminates the stream with a finish chunk.

use crate::hosted::sse::{chunks_from_events, SseChunkParser};
use crate::hosted::{http_error, transport_error};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use mahi_contracts::compute::{
    CanHandleResult, FinishReason, InferenceChunk, InferenceProvider, InferenceRequest,
    InferenceStream, ThinkingDelta, ToolCallDelta,
};
use mahi_contracts::data::{ContentBlock, MessageRole};
use mahi_contracts::error::{ContractError, InferenceError};
use mahi_contracts::types::{
    CapabilitySet, ComputeMode, ModelDescriptor, ModelSource, PerfProfile,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

/// The Anthropic Messages API endpoint.
pub const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
/// API version header value required by the Messages API.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// `max_tokens` is mandatory on the Messages API; used when the request has none.
pub const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Hosted provider speaking the Anthropic Messages API.
pub struct AnthropicProvider {
    pub api_key: String,
    pub model: String,
}

impl AnthropicProvider {
    /// Default model for mode D's Anthropic adapter.
    pub const DEFAULT_MODEL: &'static str = "claude-fable-5";

    /// Provider with the default model (`"claude-fable-5"`).
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: Self::DEFAULT_MODEL.to_string(),
        }
    }

    /// Provider with an explicit model id.
    pub fn with_model(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
        }
    }
}

/// Render a tool output value as `tool_result` content (raw text passes
/// through; structured values serialize as JSON).
fn tool_output_to_string(output: &Value) -> String {
    match output {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Anthropic `image` content blocks for any [`ContentBlock::Image`] in `m`.
fn anthropic_image_blocks(m: &mahi_contracts::data::Message) -> Vec<Value> {
    m.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Image { media_type, data } => Some(json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media_type, "data": data },
            })),
            _ => None,
        })
        .collect()
}

/// Build the Messages API request body for `req` (pure; unit-tested).
///
/// System-role messages are lifted into the top-level `system` field.
/// Assistant `ContentBlock::ToolCall`s serialize as `tool_use` content
/// blocks; tool-role `ContentBlock::ToolResult`s become `tool_result` blocks
/// inside user turns (consecutive user turns are merged by the API).
pub fn request_body(model: &str, req: &InferenceRequest) -> Value {
    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();

    for m in &req.messages {
        match m.role {
            MessageRole::System => system_parts.push(m.text_content()),
            MessageRole::Assistant => {
                let mut blocks: Vec<Value> = Vec::new();
                let mut has_tool_use = false;
                // Thinking blocks must come FIRST in the content array (and
                // carry their signature) so the API will accept a tool_use turn
                // when extended thinking is enabled.
                for b in &m.content {
                    if let ContentBlock::Thinking { thinking, signature } = b {
                        blocks.push(json!({
                            "type": "thinking",
                            "thinking": thinking,
                            "signature": signature,
                        }));
                    }
                }
                for b in &m.content {
                    match b {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            blocks.push(json!({"type": "text", "text": text}));
                        }
                        ContentBlock::ToolCall {
                            call_id,
                            tool_id,
                            args,
                        } => {
                            has_tool_use = true;
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": call_id,
                                "name": tool_id,
                                "input": args,
                            }));
                        }
                        _ => {}
                    }
                }
                if blocks.is_empty() {
                    // Plain-text turns with no structured blocks keep the simple
                    // string shape.
                    messages.push(json!({"role": "assistant", "content": m.text_content()}));
                } else if has_tool_use || blocks.len() > 1 {
                    messages.push(json!({"role": "assistant", "content": blocks}));
                } else {
                    messages.push(json!({"role": "assistant", "content": m.text_content()}));
                }
            }
            MessageRole::Tool => {
                let mut content: Vec<Value> = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolResult { call_id, output } => Some(json!({
                            "type": "tool_result",
                            "tool_use_id": call_id,
                            "content": tool_output_to_string(output),
                        })),
                        _ => None,
                    })
                    .collect();
                // A vision model sees the screenshot: attach inline images to the
                // same user turn as the tool_result.
                content.extend(anthropic_image_blocks(m));
                if content.is_empty() {
                    // Defensive: a tool message without a structured result
                    // block still surfaces as user context.
                    messages.push(json!({"role": "user", "content": m.text_content()}));
                } else {
                    messages.push(json!({"role": "user", "content": content}));
                }
            }
            MessageRole::User => {
                let images = anthropic_image_blocks(m);
                if images.is_empty() {
                    messages.push(json!({"role": "user", "content": m.text_content()}));
                } else {
                    let mut content = Vec::new();
                    let text = m.text_content();
                    if !text.is_empty() {
                        content.push(json!({"type": "text", "text": text}));
                    }
                    content.extend(images);
                    messages.push(json!({"role": "user", "content": content}));
                }
            }
        }
    }

    let mut body = json!({
        "model": model,
        "messages": messages,
        "max_tokens": req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "stream": true,
    });
    if !system_parts.is_empty() {
        body["system"] = system_parts.join("\n").into();
    }
    // Extended thinking now works alongside tools: the assistant's thinking
    // block is persisted (with its signature) and replayed first on the next
    // tool-use turn (see the assistant arm above). The one case we must still
    // avoid is enabling thinking when the history contains an assistant
    // tool_use turn WITHOUT a stored thinking block — the API would 400 because
    // the signed block can't be reconstructed. That happens only for turns
    // recorded before this feature, so we detect it and fall back gracefully.
    let unreplayable_tool_turn = req.messages.iter().any(|m| {
        m.role == MessageRole::Assistant
            && m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolCall { .. }))
            && !m
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Thinking { .. }))
    });
    let thinking = (!unreplayable_tool_turn)
        .then_some(req.thinking)
        .flatten();
    if let Some(thinking) = thinking {
        // The Messages API requires `max_tokens > budget_tokens`, so grow the
        // cap to leave room for both the reasoning and the visible answer.
        let budget = thinking.budget_tokens.max(1024);
        let max = req
            .max_tokens
            .unwrap_or(DEFAULT_MAX_TOKENS)
            .max(budget.saturating_add(DEFAULT_MAX_TOKENS));
        body["max_tokens"] = max.into();
        body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
        // Extended thinking requires the default temperature; only set an
        // explicit temperature when thinking is off.
    } else if let Some(temperature) = req.temperature {
        body["temperature"] = temperature.into();
    }
    if let Some(tools) = &req.tools {
        body["tools"] = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.id,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect::<Vec<_>>()
            .into();
    }
    body
}

fn map_stop_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" | "pause_turn" => FinishReason::Stop,
        "max_tokens" => FinishReason::MaxTokens,
        "tool_use" => FinishReason::ToolCall,
        "refusal" => FinishReason::Error,
        _ => FinishReason::Stop,
    }
}

/// Identity of a streaming `tool_use` content block, captured at
/// `content_block_start` and reused for its `input_json_delta` events.
#[derive(Default, Clone)]
struct ToolBlockIdentity {
    call_id: String,
    tool_id: String,
}

/// Stateful parser for Anthropic Messages SSE events (pure; unit-tested).
pub struct AnthropicSseParser {
    mode: ComputeMode,
    done: bool,
    stop_reason: Option<FinishReason>,
    tool_blocks: HashMap<u64, ToolBlockIdentity>,
}

impl AnthropicSseParser {
    pub fn new(mode: ComputeMode) -> Self {
        Self {
            mode,
            done: false,
            stop_reason: None,
            tool_blocks: HashMap::new(),
        }
    }

    /// Whether `message_stop` (or a terminal error) has been seen.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Parse one SSE event (`event:` type + `data:` payload) into chunks.
    pub fn handle(
        &mut self,
        event_type: &str,
        data: &str,
    ) -> Vec<Result<InferenceChunk, ContractError>> {
        if self.done {
            return Vec::new();
        }
        let value: Value = match serde_json::from_str(data.trim()) {
            Ok(v) => v,
            Err(_) if data.trim().is_empty() => Value::Null,
            Err(err) => {
                return vec![Err(ContractError::Inference(InferenceError::Provider {
                    message: format!("malformed SSE JSON: {err}"),
                    retryable: false,
                }))]
            }
        };

        match event_type {
            "content_block_start" => {
                let block = &value["content_block"];
                if block["type"].as_str() == Some("tool_use") {
                    let index = value["index"].as_u64().unwrap_or(0);
                    // `id` is always present on the wire; the synthesized
                    // fallback only guards against a malformed event so
                    // parallel calls still stay distinct.
                    let call_id = block["id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("toolu_{index}"));
                    self.tool_blocks.insert(
                        index,
                        ToolBlockIdentity {
                            call_id,
                            tool_id: block["name"].as_str().unwrap_or_default().to_string(),
                        },
                    );
                }
                Vec::new()
            }
            "content_block_delta" => {
                let delta = &value["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        let text = delta["text"].as_str().unwrap_or_default();
                        if text.is_empty() {
                            Vec::new()
                        } else {
                            vec![Ok(InferenceChunk::text(text, self.mode))]
                        }
                    }
                    Some("input_json_delta") => {
                        let index = value["index"].as_u64().unwrap_or(0);
                        let identity = self.tool_blocks.get(&index).cloned().unwrap_or_default();
                        vec![Ok(InferenceChunk {
                            delta: None,
                            tool_call_delta: Some(ToolCallDelta {
                                call_id: identity.call_id,
                                tool_id: identity.tool_id,
                                args_delta: delta["partial_json"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_string(),
                            }),
                            thinking_delta: None,
                            finish_reason: None,
                            active_mode: self.mode,
                            latency_hint_ms: None,
                        })]
                    }
                    // Extended-thinking text. Surfaced so the agent can persist
                    // the reasoning block and replay it (with its signature) on
                    // the next tool-use turn.
                    Some("thinking_delta") => {
                        let text = delta["thinking"].as_str().unwrap_or_default();
                        if text.is_empty() {
                            Vec::new()
                        } else {
                            vec![Ok(InferenceChunk::thinking(
                                ThinkingDelta {
                                    text: text.to_string(),
                                    signature: None,
                                },
                                self.mode,
                            ))]
                        }
                    }
                    // The signature that closes a thinking block; required to
                    // replay the block later.
                    Some("signature_delta") => {
                        let sig = delta["signature"].as_str().unwrap_or_default();
                        if sig.is_empty() {
                            Vec::new()
                        } else {
                            vec![Ok(InferenceChunk::thinking(
                                ThinkingDelta {
                                    text: String::new(),
                                    signature: Some(sig.to_string()),
                                },
                                self.mode,
                            ))]
                        }
                    }
                    _ => Vec::new(),
                }
            }
            "message_delta" => {
                if let Some(reason) = value["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(map_stop_reason(reason));
                }
                Vec::new()
            }
            "message_stop" => {
                self.done = true;
                vec![Ok(InferenceChunk::finish(
                    self.stop_reason.unwrap_or(FinishReason::Stop),
                    self.mode,
                ))]
            }
            "error" => {
                self.done = true;
                let kind = value["error"]["type"].as_str().unwrap_or("unknown");
                let message = value["error"]["message"]
                    .as_str()
                    .unwrap_or(data)
                    .to_string();
                vec![Err(ContractError::Inference(InferenceError::Provider {
                    message: format!("anthropic stream error ({kind}): {message}"),
                    retryable: kind == "overloaded_error" || kind == "api_error",
                }))]
            }
            // message_start, content_block_stop, ping, ...
            _ => Vec::new(),
        }
    }
}

impl SseChunkParser for AnthropicSseParser {
    fn mode(&self) -> ComputeMode {
        self.mode
    }
    fn done(&self) -> bool {
        self.done
    }
    fn handle_event(
        &mut self,
        event_type: &str,
        data: &str,
    ) -> Vec<Result<InferenceChunk, ContractError>> {
        self.handle(event_type, data)
    }
}

#[async_trait]
impl InferenceProvider for AnthropicProvider {
    fn descriptor(&self) -> ModelDescriptor {
        // TODO(contracts): capability data should come from a provider
        // catalog; conservative hosted-tier defaults for now.
        ModelDescriptor {
            id: self.model.clone(),
            display_name: format!("Hosted (Anthropic): {}", self.model),
            context_window: 200_000,
            capabilities: CapabilitySet {
                vision: true,
                tool_calling: true,
                min_context_window: 200_000,
                code_gen: true,
                thinking: true,
            },
            limitations: Vec::new(),
            size_bytes: None,
            quantization: None,
            source: ModelSource::Hosted {
                provider: "anthropic".to_string(),
            },
            perf_profile: PerfProfile {
                ttft_ms: 700,
                tok_per_sec: 70.0,
            },
        }
    }

    async fn can_handle(&self, req: &InferenceRequest) -> CanHandleResult {
        let offered = self.descriptor().capabilities;
        if req.required_caps.satisfied_by(&offered) {
            CanHandleResult::capable()
        } else {
            CanHandleResult {
                capable: false,
                missing_caps: req.required_caps.missing_from(&offered),
                escalation_hint: None,
            }
        }
    }

    async fn generate(
        &self,
        req: InferenceRequest,
        cancel: CancellationToken,
    ) -> Result<InferenceStream, ContractError> {
        let body = request_body(&self.model, &req);
        let client = reqwest::Client::new();
        let response = client
            .post(ANTHROPIC_MESSAGES_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await
            .map_err(transport_error)?;

        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            return Err(http_error("anthropic", status, detail));
        }

        let events = response.bytes_stream().eventsource();
        Ok(chunks_from_events(
            events,
            AnthropicSseParser::new(ComputeMode::Hosted),
            cancel,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mahi_contracts::compute::ToolSpec;
    use mahi_contracts::data::Message;
    use uuid::Uuid;

    fn sample_request() -> InferenceRequest {
        let conv = Uuid::new_v4();
        InferenceRequest::from_messages(vec![
            Message::text(
                conv,
                MessageRole::System,
                "Be terse.",
                ComputeMode::Hosted,
                0,
            ),
            Message::text(conv, MessageRole::User, "Hi there", ComputeMode::Hosted, 1),
            Message::text(
                conv,
                MessageRole::Assistant,
                "Hello!",
                ComputeMode::Hosted,
                2,
            ),
            Message::text(conv, MessageRole::User, "Bye", ComputeMode::Hosted, 3),
        ])
    }

    #[test]
    fn request_body_lifts_system_and_defaults_max_tokens() {
        let req = sample_request();
        let body = request_body(AnthropicProvider::DEFAULT_MODEL, &req);

        assert_eq!(body["model"], "claude-fable-5");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(body["system"], "Be terse.");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            messages.len(),
            3,
            "system message must not appear in messages[]"
        );
        assert_eq!(messages[0], json!({"role": "user", "content": "Hi there"}));
        assert_eq!(
            messages[1],
            json!({"role": "assistant", "content": "Hello!"})
        );
        assert_eq!(messages[2], json!({"role": "user", "content": "Bye"}));
    }

    #[test]
    fn request_body_maps_tools_and_explicit_params() {
        let mut req = sample_request();
        req.max_tokens = Some(2048);
        req.temperature = Some(0.2);
        req.tools = Some(vec![ToolSpec {
            id: "get_weather".to_string(),
            description: "Get the weather".to_string(),
            input_schema: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
        }]);
        let body = request_body("claude-fable-5", &req);
        assert_eq!(body["max_tokens"], 2048);
        // temperature is f32 in the contract; widened to JSON f64 it is ~0.2.
        assert!((body["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6);
        assert_eq!(
            body["tools"],
            json!([{
                "name": "get_weather",
                "description": "Get the weather",
                "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}},
            }])
        );
    }

    /// Assistant tool calls and tool results round-trip into Anthropic
    /// `tool_use` / `tool_result` content blocks for the next round.
    #[test]
    fn request_body_serializes_tool_use_and_tool_results() {
        let conv = Uuid::new_v4();
        let mut assistant = Message::text(
            conv,
            MessageRole::Assistant,
            "Let me check.",
            ComputeMode::Hosted,
            1,
        );
        assistant.content.push(ContentBlock::ToolCall {
            call_id: "toolu_1".to_string(),
            tool_id: "get_weather".to_string(),
            args: json!({"city": "Auckland"}),
        });
        assistant.content.push(ContentBlock::ToolCall {
            call_id: "toolu_2".to_string(),
            tool_id: "get_time".to_string(),
            args: json!({"tz": "Pacific/Auckland"}),
        });
        let mut result_1 = Message::text(conv, MessageRole::Tool, "", ComputeMode::Hosted, 2);
        result_1.content = vec![ContentBlock::ToolResult {
            call_id: "toolu_1".to_string(),
            output: json!({"temp_c": 21}),
        }];
        let mut result_2 = Message::text(conv, MessageRole::Tool, "", ComputeMode::Hosted, 3);
        result_2.content = vec![ContentBlock::ToolResult {
            call_id: "toolu_2".to_string(),
            output: serde_json::Value::String("09:15".to_string()),
        }];

        let req = InferenceRequest::from_messages(vec![
            Message::text(conv, MessageRole::User, "Weather?", ComputeMode::Hosted, 0),
            assistant,
            result_1,
            result_2,
        ]);
        let body = request_body("claude-fable-5", &req);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);

        // Assistant turn: text block first, then both tool_use blocks.
        assert_eq!(
            messages[1],
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Let me check."},
                    {"type": "tool_use", "id": "toolu_1", "name": "get_weather",
                     "input": {"city": "Auckland"}},
                    {"type": "tool_use", "id": "toolu_2", "name": "get_time",
                     "input": {"tz": "Pacific/Auckland"}},
                ],
            })
        );

        // Tool results: user turns carrying tool_result blocks keyed by the
        // tool_use id (consecutive user turns are merged by the API).
        assert_eq!(
            messages[2],
            json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": "{\"temp_c\":21}",
                }],
            })
        );
        assert_eq!(
            messages[3],
            json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_2",
                    "content": "09:15",
                }],
            })
        );
    }

    #[test]
    fn request_body_attaches_image_to_tool_result_for_vision() {
        let conv = Uuid::new_v4();
        let mut result = Message::text(conv, MessageRole::Tool, "", ComputeMode::Hosted, 0);
        result.content = vec![
            ContentBlock::ToolResult {
                call_id: "toolu_1".to_string(),
                output: json!("screenshot captured"),
            },
            ContentBlock::Image {
                media_type: "image/png".to_string(),
                data: "AAAA".to_string(),
            },
        ];
        let req = InferenceRequest::from_messages(vec![result]);
        let body = request_body("claude-fable-5", &req);
        let content = &body["messages"][0]["content"];
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "AAAA");
    }

    #[test]
    fn thinking_enables_block_grows_max_tokens_and_drops_temperature() {
        use mahi_contracts::compute::ThinkingConfig;
        let mut req = sample_request();
        req.temperature = Some(0.2);
        req.max_tokens = Some(1024);
        req.thinking = Some(ThinkingConfig::with_budget(4096));
        let body = request_body("claude-fable-5", &req);

        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 4096);
        // max_tokens must exceed the budget; the headroom rule grows it.
        assert!(body["max_tokens"].as_u64().unwrap() > 4096);
        // Extended thinking requires the default temperature.
        assert!(
            body.get("temperature").is_none(),
            "temperature must be omitted when thinking is on: {body}"
        );
    }

    #[test]
    fn thinking_enabled_with_tools_on_clean_history() {
        use mahi_contracts::compute::{ThinkingConfig, ToolSpec};
        let mut req = sample_request();
        req.thinking = Some(ThinkingConfig::with_budget(4096));
        req.tools = Some(vec![ToolSpec {
            id: "web_search".to_string(),
            description: "Search the web".to_string(),
            input_schema: json!({"type": "object"}),
        }]);
        let body = request_body("claude-fable-5", &req);
        // No prior tool_use turn to replay → thinking and tools coexist.
        assert_eq!(body["thinking"]["type"], "enabled");
        assert!(body["tools"].is_array());
    }

    #[test]
    fn thinking_disabled_when_prior_tool_use_lacks_a_thinking_block() {
        use mahi_contracts::compute::ThinkingConfig;
        let conv = Uuid::new_v4();
        let mut assistant =
            Message::text(conv, MessageRole::Assistant, "", ComputeMode::Hosted, 1);
        assistant.content = vec![ContentBlock::ToolCall {
            call_id: "toolu_1".to_string(),
            tool_id: "web_search".to_string(),
            args: json!({"query": "x"}),
        }];
        let mut req = InferenceRequest::from_messages(vec![
            Message::text(conv, MessageRole::User, "hi", ComputeMode::Hosted, 0),
            assistant,
        ]);
        req.thinking = Some(ThinkingConfig::with_budget(4096));
        let body = request_body("claude-fable-5", &req);
        // The unreplayable tool_use turn would 400 if thinking were enabled.
        assert!(
            body.get("thinking").is_none(),
            "thinking must be disabled when a prior tool_use turn has no signed thinking block: {body}"
        );
    }

    #[test]
    fn request_body_replays_stored_thinking_block_first_with_signature() {
        use mahi_contracts::compute::ThinkingConfig;
        let conv = Uuid::new_v4();
        let mut assistant =
            Message::text(conv, MessageRole::Assistant, "answer", ComputeMode::Hosted, 1);
        assistant.content = vec![
            ContentBlock::Thinking {
                thinking: "let me search".to_string(),
                signature: "sig_abc".to_string(),
            },
            ContentBlock::ToolCall {
                call_id: "toolu_1".to_string(),
                tool_id: "web_search".to_string(),
                args: json!({"query": "x"}),
            },
        ];
        let mut result =
            Message::text(conv, MessageRole::Tool, "", ComputeMode::Hosted, 2);
        result.content = vec![ContentBlock::ToolResult {
            call_id: "toolu_1".to_string(),
            output: json!({"results": []}),
        }];
        let mut req = InferenceRequest::from_messages(vec![
            Message::text(conv, MessageRole::User, "search x", ComputeMode::Hosted, 0),
            assistant,
            result,
        ]);
        req.thinking = Some(ThinkingConfig::with_budget(4096));
        let body = request_body("claude-fable-5", &req);
        let assistant_msg = &body["messages"][1];
        // Thinking block replayed FIRST with its signature, then the tool_use.
        assert_eq!(assistant_msg["content"][0]["type"], "thinking");
        assert_eq!(assistant_msg["content"][0]["thinking"], "let me search");
        assert_eq!(assistant_msg["content"][0]["signature"], "sig_abc");
        assert_eq!(assistant_msg["content"][1]["type"], "tool_use");
        // The stored block is replayable, so thinking stays enabled.
        assert_eq!(body["thinking"]["type"], "enabled");
    }

    #[test]
    fn no_thinking_keeps_temperature_and_max_tokens() {
        let mut req = sample_request();
        req.temperature = Some(0.3);
        req.max_tokens = Some(512);
        let body = request_body("claude-fable-5", &req);
        assert!(body.get("thinking").is_none());
        assert_eq!(body["max_tokens"], 512);
        assert!((body["temperature"].as_f64().unwrap() - 0.3).abs() < 1e-6);
    }

    #[test]
    fn default_model_is_claude_fable_5() {
        let p = AnthropicProvider::new("sk-test");
        assert_eq!(p.model, "claude-fable-5");
        let q = AnthropicProvider::with_model("sk-test", "claude-opus-4-8");
        assert_eq!(q.model, "claude-opus-4-8");
    }

    #[test]
    fn parser_surfaces_thinking_text_and_signature() {
        let mut parser = AnthropicSseParser::new(ComputeMode::Hosted);
        let mut chunks = Vec::new();
        for (event, data) in [
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me "}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reason."}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_xyz"}}"#,
            ),
            ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        ] {
            chunks.extend(parser.handle(event, data));
        }
        let chunks: Vec<_> = chunks.into_iter().map(Result::unwrap).collect();
        let text: String = chunks
            .iter()
            .filter_map(|c| c.thinking_delta.as_ref())
            .map(|t| t.text.as_str())
            .collect();
        assert_eq!(text, "Let me reason.");
        let sig = chunks
            .iter()
            .filter_map(|c| c.thinking_delta.as_ref())
            .find_map(|t| t.signature.clone());
        assert_eq!(sig.as_deref(), Some("sig_xyz"));
    }

    #[test]
    fn parser_text_deltas_then_message_stop() {
        let mut parser = AnthropicSseParser::new(ComputeMode::Hosted);
        let mut chunks = Vec::new();
        for (event, data) in [
            (
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1","role":"assistant"}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            ("ping", r#"{"type":"ping"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":12}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ] {
            chunks.extend(parser.handle(event, data));
        }
        let chunks: Vec<_> = chunks.into_iter().map(Result::unwrap).collect();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].delta.as_deref(), Some("Hello"));
        assert_eq!(chunks[1].delta.as_deref(), Some(" world"));
        assert_eq!(chunks[2].finish_reason, Some(FinishReason::Stop));
        assert!(parser.is_done());
    }

    #[test]
    fn parser_tool_use_passthrough() {
        let mut parser = AnthropicSseParser::new(ComputeMode::Hosted);
        let mut chunks = Vec::new();
        for (event, data) in [
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"Auckland\"}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ] {
            chunks.extend(parser.handle(event, data));
        }
        let chunks: Vec<_> = chunks.into_iter().map(Result::unwrap).collect();
        assert_eq!(chunks.len(), 3);
        let deltas: Vec<&ToolCallDelta> = chunks
            .iter()
            .filter_map(|c| c.tool_call_delta.as_ref())
            .collect();
        assert_eq!(deltas.len(), 2);
        assert!(deltas
            .iter()
            .all(|d| d.call_id == "toolu_1" && d.tool_id == "get_weather"));
        let args: String = deltas.iter().map(|d| d.args_delta.as_str()).collect();
        assert_eq!(args, r#"{"city":"Auckland"}"#);
        assert_eq!(chunks[2].finish_reason, Some(FinishReason::ToolCall));
    }

    /// Two parallel tool_use blocks in one assistant message: each block has
    /// its own index, and input_json_delta events interleave between them.
    #[test]
    fn parser_parallel_tool_use_blocks() {
        let mut parser = AnthropicSseParser::new(ComputeMode::Hosted);
        let mut chunks = Vec::new();
        for (event, data) in [
            (
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1","role":"assistant"}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_a","name":"get_weather","input":{}}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_b","name":"get_time","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"tz\":\"Pacific/"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"Auckland\"}"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"Auckland\"}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ] {
            chunks.extend(parser.handle(event, data));
        }
        let chunks: Vec<_> = chunks.into_iter().map(Result::unwrap).collect();

        let args_for = |call_id: &str| -> String {
            chunks
                .iter()
                .filter_map(|c| c.tool_call_delta.as_ref())
                .filter(|d| d.call_id == call_id)
                .map(|d| d.args_delta.as_str())
                .collect()
        };
        assert_eq!(args_for("toolu_a"), r#"{"city":"Auckland"}"#);
        assert_eq!(args_for("toolu_b"), r#"{"tz":"Pacific/Auckland"}"#);
        assert!(chunks
            .iter()
            .filter_map(|c| c.tool_call_delta.as_ref())
            .all(|d| (d.call_id == "toolu_a" && d.tool_id == "get_weather")
                || (d.call_id == "toolu_b" && d.tool_id == "get_time")));
        assert_eq!(
            chunks.last().unwrap().finish_reason,
            Some(FinishReason::ToolCall)
        );
    }

    #[test]
    fn parser_error_event_maps_to_provider_error() {
        let mut parser = AnthropicSseParser::new(ComputeMode::Hosted);
        let chunks = parser.handle(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        );
        assert_eq!(chunks.len(), 1);
        match chunks.into_iter().next().unwrap() {
            Err(ContractError::Inference(InferenceError::Provider { message, retryable })) => {
                assert!(retryable);
                assert!(message.contains("Overloaded"));
            }
            other => panic!("expected provider error, got {other:?}"),
        }
        assert!(parser.is_done());
    }

    /// End-to-end: canned SSE bytes (with `event:` lines) -> eventsource ->
    /// parser -> chunks.
    #[tokio::test]
    async fn sse_bytes_end_to_end() {
        use crate::hosted::sse::chunks_from_events;
        use futures::{stream, StreamExt};
        use std::convert::Infallible;

        let raw = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Kia\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" ora\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let events = stream::iter(vec![Ok::<_, Infallible>(raw.as_bytes())]).eventsource();
        let chunks: Vec<_> = chunks_from_events(
            events,
            AnthropicSseParser::new(ComputeMode::Hosted),
            CancellationToken::new(),
        )
        .collect()
        .await;

        let chunks: Vec<_> = chunks.into_iter().map(Result::unwrap).collect();
        let text: String = chunks.iter().filter_map(|c| c.delta.as_deref()).collect();
        assert_eq!(text, "Kia ora");
        assert_eq!(
            chunks.last().unwrap().finish_reason,
            Some(FinishReason::Stop)
        );
        assert!(chunks.iter().all(|c| c.active_mode == ComputeMode::Hosted));
    }
}
