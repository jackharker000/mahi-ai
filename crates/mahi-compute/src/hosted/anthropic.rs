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
    InferenceStream, ToolCallDelta,
};
use mahi_contracts::data::MessageRole;
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
        Self { api_key: api_key.into(), model: Self::DEFAULT_MODEL.to_string() }
    }

    /// Provider with an explicit model id.
    pub fn with_model(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self { api_key: api_key.into(), model: model.into() }
    }
}

/// Build the Messages API request body for `req` (pure; unit-tested).
///
/// System-role messages are lifted into the top-level `system` field; tool
/// role messages are folded into user turns.
pub fn request_body(model: &str, req: &InferenceRequest) -> Value {
    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();

    for m in &req.messages {
        match m.role {
            MessageRole::System => system_parts.push(m.text_content()),
            MessageRole::Assistant => {
                messages.push(json!({"role": "assistant", "content": m.text_content()}))
            }
            // TODO(contracts): tool results should map to `tool_result`
            // content blocks keyed by call id; `Message` does not carry
            // enough structure for that yet, so fold them into user turns.
            MessageRole::User | MessageRole::Tool => {
                messages.push(json!({"role": "user", "content": m.text_content()}))
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
    if let Some(temperature) = req.temperature {
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
        Self { mode, done: false, stop_reason: None, tool_blocks: HashMap::new() }
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
                    self.tool_blocks.insert(
                        index,
                        ToolBlockIdentity {
                            call_id: block["id"].as_str().unwrap_or_default().to_string(),
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
                        let identity =
                            self.tool_blocks.get(&index).cloned().unwrap_or_default();
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
                            finish_reason: None,
                            active_mode: self.mode,
                            latency_hint_ms: None,
                        })]
                    }
                    _ => Vec::new(), // e.g. thinking_delta — not surfaced
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
                let message = value["error"]["message"].as_str().unwrap_or(data).to_string();
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
            },
            limitations: Vec::new(),
            size_bytes: None,
            quantization: None,
            source: ModelSource::Hosted { provider: "anthropic".to_string() },
            perf_profile: PerfProfile { ttft_ms: 700, tok_per_sec: 70.0 },
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
        Ok(chunks_from_events(events, AnthropicSseParser::new(ComputeMode::Hosted), cancel))
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
            Message::text(conv, MessageRole::System, "Be terse.", ComputeMode::Hosted, 0),
            Message::text(conv, MessageRole::User, "Hi there", ComputeMode::Hosted, 1),
            Message::text(conv, MessageRole::Assistant, "Hello!", ComputeMode::Hosted, 2),
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
        assert_eq!(messages.len(), 3, "system message must not appear in messages[]");
        assert_eq!(messages[0], json!({"role": "user", "content": "Hi there"}));
        assert_eq!(messages[1], json!({"role": "assistant", "content": "Hello!"}));
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
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(
            body["tools"],
            json!([{
                "name": "get_weather",
                "description": "Get the weather",
                "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}},
            }])
        );
    }

    #[test]
    fn default_model_is_claude_fable_5() {
        let p = AnthropicProvider::new("sk-test");
        assert_eq!(p.model, "claude-fable-5");
        let q = AnthropicProvider::with_model("sk-test", "claude-opus-4-8");
        assert_eq!(q.model, "claude-opus-4-8");
    }

    #[test]
    fn parser_text_deltas_then_message_stop() {
        let mut parser = AnthropicSseParser::new(ComputeMode::Hosted);
        let mut chunks = Vec::new();
        for (event, data) in [
            ("message_start", r#"{"type":"message_start","message":{"id":"msg_1","role":"assistant"}}"#),
            ("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#),
            ("ping", r#"{"type":"ping"}"#),
            ("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#),
            ("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}"#),
            ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":12}}"#),
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
            ("content_block_start", r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{}}}"#),
            ("content_block_delta", r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#),
            ("content_block_delta", r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"Auckland\"}"}}"#),
            ("content_block_stop", r#"{"type":"content_block_stop","index":1}"#),
            ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ] {
            chunks.extend(parser.handle(event, data));
        }
        let chunks: Vec<_> = chunks.into_iter().map(Result::unwrap).collect();
        assert_eq!(chunks.len(), 3);
        let deltas: Vec<&ToolCallDelta> =
            chunks.iter().filter_map(|c| c.tool_call_delta.as_ref()).collect();
        assert_eq!(deltas.len(), 2);
        assert!(deltas.iter().all(|d| d.call_id == "toolu_1" && d.tool_id == "get_weather"));
        let args: String = deltas.iter().map(|d| d.args_delta.as_str()).collect();
        assert_eq!(args, r#"{"city":"Auckland"}"#);
        assert_eq!(chunks[2].finish_reason, Some(FinishReason::ToolCall));
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
        let events =
            stream::iter(vec![Ok::<_, Infallible>(raw.as_bytes())]).eventsource();
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
        assert_eq!(chunks.last().unwrap().finish_reason, Some(FinishReason::Stop));
        assert!(chunks.iter().all(|c| c.active_mode == ComputeMode::Hosted));
    }
}
