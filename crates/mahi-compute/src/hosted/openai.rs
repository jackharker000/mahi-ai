//! OpenAI-compatible chat-completions provider (the canonical hosted wire
//! format per domain ③ §5 — Together/Fireworks/Groq/Mistral and an Ollama
//! Mac daemon all speak it).

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

/// Hosted provider speaking the OpenAI chat-completions wire format.
///
/// POSTs `{base_url}/v1/chat/completions` with `stream: true` and parses the
/// SSE `data:` lines into [`InferenceChunk`]s.
pub struct OpenAiCompatProvider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl OpenAiCompatProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self { base_url: base_url.into(), api_key: api_key.into(), model: model.into() }
    }

    /// The streaming chat-completions endpoint for `base_url`.
    pub fn chat_completions_url(&self) -> String {
        format!("{}/v1/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

/// Build the chat-completions request body for `req` (pure; unit-tested).
pub fn request_body(model: &str, req: &InferenceRequest) -> Value {
    let messages: Vec<Value> = req
        .messages
        .iter()
        .map(|m| {
            let role = match m.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::System => "system",
                MessageRole::Tool => "tool",
            };
            // TODO(contracts): ContentBlock::{ToolCall,ToolResult,ArtifactRef}
            // have no lossless mapping here without per-call ids threaded
            // through `Message`; flattened to text for now.
            json!({ "role": role, "content": m.text_content() })
        })
        .collect();

    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
    });
    if let Some(max_tokens) = req.max_tokens {
        body["max_tokens"] = max_tokens.into();
    }
    if let Some(temperature) = req.temperature {
        body["temperature"] = temperature.into();
    }
    if let Some(tools) = &req.tools {
        body["tools"] = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.id,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                })
            })
            .collect::<Vec<_>>()
            .into();
    }
    body
}

fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::MaxTokens,
        "tool_calls" => FinishReason::ToolCall,
        "content_filter" => FinishReason::Error,
        _ => FinishReason::Stop,
    }
}

/// Identity of a streamed tool call (id/name arrive only on its first delta).
#[derive(Default, Clone)]
struct ToolCallIdentity {
    call_id: String,
    tool_id: String,
}

/// Stateful parser for OpenAI-compatible SSE `data:` payloads (pure; unit-tested).
pub struct OpenAiSseParser {
    mode: ComputeMode,
    done: bool,
    finish_emitted: bool,
    tool_calls: HashMap<u64, ToolCallIdentity>,
}

impl OpenAiSseParser {
    pub fn new(mode: ComputeMode) -> Self {
        Self { mode, done: false, finish_emitted: false, tool_calls: HashMap::new() }
    }

    /// Whether the stream has reached `[DONE]`.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Parse one SSE `data:` payload into chunks.
    pub fn handle_data(&mut self, data: &str) -> Vec<Result<InferenceChunk, ContractError>> {
        if self.done {
            return Vec::new();
        }
        let data = data.trim();
        if data.is_empty() {
            return Vec::new();
        }
        if data == "[DONE]" {
            self.done = true;
            if !self.finish_emitted {
                self.finish_emitted = true;
                return vec![Ok(InferenceChunk::finish(FinishReason::Stop, self.mode))];
            }
            return Vec::new();
        }

        let value: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(err) => {
                return vec![Err(ContractError::Inference(InferenceError::Provider {
                    message: format!("malformed SSE JSON: {err}"),
                    retryable: false,
                }))]
            }
        };

        let mut out = Vec::new();
        let choice = &value["choices"][0];
        let delta = &choice["delta"];

        if let Some(text) = delta["content"].as_str() {
            if !text.is_empty() {
                out.push(Ok(InferenceChunk::text(text, self.mode)));
            }
        }

        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                let index = call["index"].as_u64().unwrap_or(0);
                let identity = self.tool_calls.entry(index).or_default();
                if let Some(id) = call["id"].as_str() {
                    identity.call_id = id.to_string();
                }
                if let Some(name) = call["function"]["name"].as_str() {
                    identity.tool_id = name.to_string();
                }
                let args_delta = call["function"]["arguments"].as_str().unwrap_or_default();
                out.push(Ok(InferenceChunk {
                    delta: None,
                    tool_call_delta: Some(ToolCallDelta {
                        call_id: identity.call_id.clone(),
                        tool_id: identity.tool_id.clone(),
                        args_delta: args_delta.to_string(),
                    }),
                    finish_reason: None,
                    active_mode: self.mode,
                    latency_hint_ms: None,
                }));
            }
        }

        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish_emitted = true;
            out.push(Ok(InferenceChunk::finish(map_finish_reason(reason), self.mode)));
        }
        out
    }
}

impl SseChunkParser for OpenAiSseParser {
    fn mode(&self) -> ComputeMode {
        self.mode
    }
    fn done(&self) -> bool {
        self.done
    }
    fn handle_event(
        &mut self,
        _event_type: &str,
        data: &str,
    ) -> Vec<Result<InferenceChunk, ContractError>> {
        self.handle_data(data)
    }
}

#[async_trait]
impl InferenceProvider for OpenAiCompatProvider {
    fn descriptor(&self) -> ModelDescriptor {
        // TODO(contracts): real capability/context data should come from a
        // provider catalog; these are conservative hosted-tier defaults.
        ModelDescriptor {
            id: self.model.clone(),
            display_name: format!("Hosted (OpenAI-compat): {}", self.model),
            context_window: 128_000,
            capabilities: CapabilitySet {
                vision: true,
                tool_calling: true,
                min_context_window: 128_000,
                code_gen: true,
            },
            limitations: Vec::new(),
            size_bytes: None,
            quantization: None,
            source: ModelSource::Hosted { provider: "openai-compat".to_string() },
            perf_profile: PerfProfile { ttft_ms: 600, tok_per_sec: 80.0 },
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
            .post(self.chat_completions_url())
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(transport_error)?;

        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            return Err(http_error("openai-compat", status, detail));
        }

        let events = response.bytes_stream().eventsource();
        Ok(chunks_from_events(events, OpenAiSseParser::new(ComputeMode::Hosted), cancel))
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
        let mut req = InferenceRequest::from_messages(vec![
            Message::text(conv, MessageRole::System, "Be terse.", ComputeMode::Hosted, 0),
            Message::text(conv, MessageRole::User, "Hi there", ComputeMode::Hosted, 1),
        ]);
        req.max_tokens = Some(256);
        req.temperature = Some(0.5);
        req
    }

    #[test]
    fn request_body_basic_shape() {
        let req = sample_request();
        let body = request_body("test-model", &req);

        assert_eq!(body["model"], "test-model");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 256);
        assert_eq!(body["temperature"], 0.5);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], json!({"role": "system", "content": "Be terse."}));
        assert_eq!(messages[1], json!({"role": "user", "content": "Hi there"}));
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn request_body_maps_tools() {
        let mut req = sample_request();
        req.tools = Some(vec![ToolSpec {
            id: "get_weather".to_string(),
            description: "Get the weather".to_string(),
            input_schema: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
        }]);
        let body = request_body("test-model", &req);
        assert_eq!(
            body["tools"],
            json!([{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get the weather",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
                }
            }])
        );
    }

    #[test]
    fn endpoint_trims_trailing_slash() {
        let p = OpenAiCompatProvider::new("https://api.example.com/", "k", "m");
        assert_eq!(p.chat_completions_url(), "https://api.example.com/v1/chat/completions");
    }

    #[test]
    fn parser_text_deltas_and_done() {
        let mut parser = OpenAiSseParser::new(ComputeMode::Hosted);
        let mut chunks = Vec::new();
        for data in [
            r#"{"id":"c1","choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"},"finish_reason":null}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":null}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ] {
            chunks.extend(parser.handle_data(data));
        }
        let chunks: Vec<_> = chunks.into_iter().map(Result::unwrap).collect();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].delta.as_deref(), Some("Hel"));
        assert_eq!(chunks[1].delta.as_deref(), Some("lo"));
        assert_eq!(chunks[2].finish_reason, Some(FinishReason::Stop));
        assert!(chunks.iter().all(|c| c.active_mode == ComputeMode::Hosted));
        assert!(parser.is_done());
    }

    #[test]
    fn parser_done_without_finish_reason_synthesizes_stop() {
        let mut parser = OpenAiSseParser::new(ComputeMode::Hosted);
        let chunks = parser.handle_data("[DONE]");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].as_ref().unwrap().finish_reason, Some(FinishReason::Stop));
    }

    #[test]
    fn parser_tool_call_deltas_carry_identity() {
        let mut parser = OpenAiSseParser::new(ComputeMode::Hosted);
        let mut chunks = Vec::new();
        for data in [
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Auckland\"}"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ] {
            chunks.extend(parser.handle_data(data));
        }
        let chunks: Vec<_> = chunks.into_iter().map(Result::unwrap).collect();
        assert_eq!(chunks.len(), 4);

        let deltas: Vec<&ToolCallDelta> =
            chunks.iter().filter_map(|c| c.tool_call_delta.as_ref()).collect();
        assert_eq!(deltas.len(), 3);
        // Identity persists across continuation deltas that omit id/name.
        assert!(deltas.iter().all(|d| d.call_id == "call_1" && d.tool_id == "get_weather"));
        let args: String = deltas.iter().map(|d| d.args_delta.as_str()).collect();
        assert_eq!(args, r#"{"city":"Auckland"}"#);
        assert_eq!(chunks[3].finish_reason, Some(FinishReason::ToolCall));
    }

    #[test]
    fn parser_malformed_json_yields_provider_error() {
        let mut parser = OpenAiSseParser::new(ComputeMode::Hosted);
        let chunks = parser.handle_data("{not json");
        assert_eq!(chunks.len(), 1);
        match chunks.into_iter().next().unwrap() {
            Err(ContractError::Inference(InferenceError::Provider { retryable, .. })) => {
                assert!(!retryable)
            }
            other => panic!("expected provider error, got {other:?}"),
        }
    }

    /// End-to-end: canned SSE bytes -> eventsource -> parser -> chunks.
    #[tokio::test]
    async fn sse_bytes_end_to_end() {
        use crate::hosted::sse::chunks_from_events;
        use futures::{stream, StreamExt};
        use std::convert::Infallible;

        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\" there\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let events =
            stream::iter(vec![Ok::<_, Infallible>(raw.as_bytes())]).eventsource();
        let chunks: Vec<_> = chunks_from_events(
            events,
            OpenAiSseParser::new(ComputeMode::Hosted),
            CancellationToken::new(),
        )
        .collect()
        .await;

        let chunks: Vec<_> = chunks.into_iter().map(Result::unwrap).collect();
        let text: String = chunks.iter().filter_map(|c| c.delta.as_deref()).collect();
        assert_eq!(text, "Hi there");
        assert_eq!(chunks.last().unwrap().finish_reason, Some(FinishReason::Stop));
        assert!(chunks.iter().all(|c| c.active_mode == ComputeMode::Hosted));
    }
}
