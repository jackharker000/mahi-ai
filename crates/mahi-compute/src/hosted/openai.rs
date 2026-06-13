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
use mahi_contracts::data::{ContentBlock, MessageRole};
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
    /// Mode stamped on every emitted chunk (defaults to [`ComputeMode::Hosted`]).
    mode: ComputeMode,
}

impl OpenAiCompatProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            mode: ComputeMode::Hosted,
        }
    }

    /// Same provider, but chunks are stamped with `mode` instead of `Hosted`
    /// (e.g. [`ComputeMode::OnDevice`] for a managed local `llama-server`).
    pub fn with_mode(mut self, mode: ComputeMode) -> Self {
        self.mode = mode;
        self
    }

    /// The compute mode stamped on emitted chunks.
    pub fn mode(&self) -> ComputeMode {
        self.mode
    }

    /// The streaming chat-completions endpoint for `base_url`.
    pub fn chat_completions_url(&self) -> String {
        format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        )
    }
}

/// Render a tool output value as the string content of a `role:"tool"`
/// message (raw text passes through; structured values serialize as JSON).
fn tool_output_to_string(output: &Value) -> String {
    match output {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// OpenAI `image_url` content parts (data URLs) for any [`ContentBlock::Image`].
fn openai_image_blocks(m: &mahi_contracts::data::Message) -> Vec<Value> {
    m.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Image { media_type, data } => Some(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{media_type};base64,{data}") },
            })),
            _ => None,
        })
        .collect()
}

/// Build the chat-completions request body for `req` (pure; unit-tested).
///
/// Assistant `ContentBlock::ToolCall`s serialize as OpenAI `tool_calls`
/// (function name = tool id, arguments = JSON string); each tool-role
/// `ContentBlock::ToolResult` becomes its own `role:"tool"` message keyed by
/// `tool_call_id`, which is the shape the next round's model expects.
pub fn request_body(model: &str, req: &InferenceRequest) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    for m in &req.messages {
        match m.role {
            MessageRole::System => {
                messages.push(json!({ "role": "system", "content": m.text_content() }))
            }
            MessageRole::User => {
                let images = openai_image_blocks(m);
                if images.is_empty() {
                    messages.push(json!({ "role": "user", "content": m.text_content() }));
                } else {
                    let mut content = Vec::new();
                    let text = m.text_content();
                    if !text.is_empty() {
                        content.push(json!({ "type": "text", "text": text }));
                    }
                    content.extend(images);
                    messages.push(json!({ "role": "user", "content": content }));
                }
            }
            MessageRole::Assistant => {
                let tool_calls: Vec<Value> = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolCall {
                            call_id,
                            tool_id,
                            args,
                        } => Some(json!({
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": tool_id,
                                "arguments": serde_json::to_string(args)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            }
                        })),
                        _ => None,
                    })
                    .collect();
                let text = m.text_content();
                let mut msg = json!({ "role": "assistant" });
                // OpenAI allows content:null only alongside tool_calls.
                msg["content"] = if text.is_empty() && !tool_calls.is_empty() {
                    Value::Null
                } else {
                    text.into()
                };
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = tool_calls.into();
                }
                messages.push(msg);
            }
            MessageRole::Tool => {
                let mut emitted = false;
                for b in &m.content {
                    if let ContentBlock::ToolResult { call_id, output } = b {
                        emitted = true;
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "content": tool_output_to_string(output),
                        }));
                    }
                }
                // OpenAI tool messages are text-only; surface any inline images
                // (a screenshot) as a follow-up user message so vision models
                // can see them.
                let images = openai_image_blocks(m);
                if !images.is_empty() {
                    let mut content = vec![
                        json!({ "type": "text", "text": "Screenshot from the tool result above:" }),
                    ];
                    content.extend(images);
                    messages.push(json!({ "role": "user", "content": content }));
                    emitted = true;
                }
                // Defensive: a tool message without a structured result block
                // still surfaces as context rather than being dropped.
                if !emitted {
                    messages.push(json!({ "role": "user", "content": m.text_content() }));
                }
            }
        }
    }

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
    // Only hint reasoning on tool-free turns: many OpenAI-compatible servers
    // (and llama.cpp) reject or mishandle `reasoning_effort` alongside a
    // `tools` array, which would break the agent's tool loop. Tool turns send
    // a plain request instead.
    if req.tools.is_none() {
        if let Some(thinking) = &req.thinking {
            // OpenAI-compatible reasoning endpoints (o-series, vLLM, Ollama's
            // thinking models) accept a coarse `reasoning_effort` tier rather
            // than a token budget; map the budget onto low/medium/high.
            body["reasoning_effort"] = thinking.reasoning_effort().into();
        }
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
        Self {
            mode,
            done: false,
            finish_emitted: false,
            tool_calls: HashMap::new(),
        }
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
                // Some OpenAI-compat servers (e.g. local llama.cpp builds)
                // omit `id`; synthesize a per-index id so parallel calls stay
                // distinct. A real id on the first fragment overrides it.
                let identity = self
                    .tool_calls
                    .entry(index)
                    .or_insert_with(|| ToolCallIdentity {
                        call_id: format!("call_{index}"),
                        tool_id: String::new(),
                    });
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
                    thinking_delta: None,
                    finish_reason: None,
                    active_mode: self.mode,
                    latency_hint_ms: None,
                }));
            }
        }

        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish_emitted = true;
            out.push(Ok(InferenceChunk::finish(
                map_finish_reason(reason),
                self.mode,
            )));
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
                thinking: true,
            },
            limitations: Vec::new(),
            size_bytes: None,
            quantization: None,
            source: ModelSource::Hosted {
                provider: "openai-compat".to_string(),
            },
            perf_profile: PerfProfile {
                ttft_ms: 600,
                tok_per_sec: 80.0,
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
        Ok(chunks_from_events(
            events,
            OpenAiSseParser::new(self.mode),
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
        let mut req = InferenceRequest::from_messages(vec![
            Message::text(
                conv,
                MessageRole::System,
                "Be terse.",
                ComputeMode::Hosted,
                0,
            ),
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
        assert_eq!(
            messages[0],
            json!({"role": "system", "content": "Be terse."})
        );
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

    /// Assistant tool calls and tool results round-trip into the OpenAI
    /// `tool_calls` / `role:"tool"` wire shape for the next round.
    #[test]
    fn request_body_serializes_tool_calls_and_results() {
        let conv = Uuid::new_v4();
        let mut assistant = Message::text(
            conv,
            MessageRole::Assistant,
            "Checking two cities.",
            ComputeMode::Hosted,
            1,
        );
        assistant.content.push(ContentBlock::ToolCall {
            call_id: "call_1".to_string(),
            tool_id: "get_weather".to_string(),
            args: json!({"city": "Auckland"}),
        });
        assistant.content.push(ContentBlock::ToolCall {
            call_id: "call_2".to_string(),
            tool_id: "get_weather".to_string(),
            args: json!({"city": "Wellington"}),
        });
        let mut result_1 = Message::text(conv, MessageRole::Tool, "", ComputeMode::Hosted, 2);
        result_1.content = vec![ContentBlock::ToolResult {
            call_id: "call_1".to_string(),
            output: json!({"temp_c": 21}),
        }];
        let mut result_2 = Message::text(conv, MessageRole::Tool, "", ComputeMode::Hosted, 3);
        result_2.content = vec![ContentBlock::ToolResult {
            call_id: "call_2".to_string(),
            output: serde_json::Value::String("18C and windy".to_string()),
        }];

        let req = InferenceRequest::from_messages(vec![
            Message::text(conv, MessageRole::User, "Weather?", ComputeMode::Hosted, 0),
            assistant,
            result_1,
            result_2,
        ]);
        let body = request_body("test-model", &req);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);

        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], "Checking two cities.");
        assert_eq!(
            messages[1]["tool_calls"],
            json!([
                {
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Auckland\"}"},
                },
                {
                    "id": "call_2",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Wellington\"}"},
                },
            ])
        );

        // Each tool result is its own role:"tool" message keyed by call id;
        // string outputs pass through raw, structured outputs as JSON text.
        assert_eq!(
            messages[2],
            json!({"role": "tool", "tool_call_id": "call_1", "content": "{\"temp_c\":21}"})
        );
        assert_eq!(
            messages[3],
            json!({"role": "tool", "tool_call_id": "call_2", "content": "18C and windy"})
        );
    }

    #[test]
    fn request_body_appends_image_as_user_message_after_tool_result() {
        let conv = Uuid::new_v4();
        let mut result = Message::text(conv, MessageRole::Tool, "", ComputeMode::Hosted, 0);
        result.content = vec![
            ContentBlock::ToolResult {
                call_id: "call_1".to_string(),
                output: json!("captured"),
            },
            ContentBlock::Image {
                media_type: "image/png".to_string(),
                data: "AAAA".to_string(),
            },
        ];
        let req = InferenceRequest::from_messages(vec![result]);
        let body = request_body("test-model", &req);
        let messages = body["messages"].as_array().unwrap();
        // role:"tool" text result, then a role:"user" message carrying the image.
        assert_eq!(messages[0]["role"], "tool");
        assert_eq!(messages[1]["role"], "user");
        let parts = messages[1]["content"].as_array().unwrap();
        assert_eq!(parts.last().unwrap()["type"], "image_url");
        assert_eq!(
            parts.last().unwrap()["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
    }

    /// A tool-call-only assistant turn serializes with `content: null`.
    #[test]
    fn request_body_tool_call_without_text_has_null_content() {
        let conv = Uuid::new_v4();
        let mut assistant = Message::text(conv, MessageRole::Assistant, "", ComputeMode::Hosted, 0);
        assistant.content = vec![ContentBlock::ToolCall {
            call_id: "call_1".to_string(),
            tool_id: "ping".to_string(),
            args: json!({}),
        }];
        let req = InferenceRequest::from_messages(vec![assistant]);
        let body = request_body("test-model", &req);
        let msg = &body["messages"][0];
        assert!(msg["content"].is_null());
        assert_eq!(msg["tool_calls"][0]["function"]["name"], "ping");
    }

    #[test]
    fn thinking_maps_to_reasoning_effort_tier() {
        use mahi_contracts::compute::ThinkingConfig;
        let mut req = sample_request();
        req.thinking = Some(ThinkingConfig::with_budget(4096));
        assert_eq!(request_body("m", &req)["reasoning_effort"], "medium");

        req.thinking = Some(ThinkingConfig::with_budget(1024));
        assert_eq!(request_body("m", &req)["reasoning_effort"], "low");

        req.thinking = Some(ThinkingConfig::with_budget(16_000));
        assert_eq!(request_body("m", &req)["reasoning_effort"], "high");
    }

    #[test]
    fn no_thinking_omits_reasoning_effort() {
        let req = sample_request();
        assert!(request_body("m", &req).get("reasoning_effort").is_none());
    }

    #[test]
    fn reasoning_effort_is_omitted_when_tools_are_present() {
        use mahi_contracts::compute::{ThinkingConfig, ToolSpec};
        let mut req = sample_request();
        req.thinking = Some(ThinkingConfig::with_budget(4096));
        req.tools = Some(vec![ToolSpec {
            id: "web_search".to_string(),
            description: "Search the web".to_string(),
            input_schema: json!({"type": "object"}),
        }]);
        let body = request_body("m", &req);
        // Tool turns must not carry reasoning_effort (breaks the tool loop on
        // many OpenAI-compatible / llama.cpp servers).
        assert!(
            body.get("reasoning_effort").is_none(),
            "reasoning_effort must be omitted when tools are present: {body}"
        );
        assert!(body["tools"].is_array());
    }

    #[test]
    fn endpoint_trims_trailing_slash() {
        let p = OpenAiCompatProvider::new("https://api.example.com/", "k", "m");
        assert_eq!(
            p.chat_completions_url(),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn mode_defaults_to_hosted_and_with_mode_overrides() {
        let p = OpenAiCompatProvider::new("http://127.0.0.1:8080", "", "m");
        assert_eq!(p.mode(), ComputeMode::Hosted);
        let p = p.with_mode(ComputeMode::OnDevice);
        assert_eq!(p.mode(), ComputeMode::OnDevice);
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
        assert_eq!(
            chunks[0].as_ref().unwrap().finish_reason,
            Some(FinishReason::Stop)
        );
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

        let deltas: Vec<&ToolCallDelta> = chunks
            .iter()
            .filter_map(|c| c.tool_call_delta.as_ref())
            .collect();
        assert_eq!(deltas.len(), 3);
        // Identity persists across continuation deltas that omit id/name.
        assert!(deltas
            .iter()
            .all(|d| d.call_id == "call_1" && d.tool_id == "get_weather"));
        let args: String = deltas.iter().map(|d| d.args_delta.as_str()).collect();
        assert_eq!(args, r#"{"city":"Auckland"}"#);
        assert_eq!(chunks[3].finish_reason, Some(FinishReason::ToolCall));
    }

    /// Two parallel tool calls in one assistant turn: fragments interleave by
    /// `index`, ids arrive only on each call's first fragment.
    #[test]
    fn parser_parallel_tool_calls_interleaved() {
        let mut parser = OpenAiSseParser::new(ComputeMode::Hosted);
        let mut chunks = Vec::new();
        for data in [
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"get_time","arguments":""}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":\"Auck"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"tz\":\"Pacific/"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"land\"}"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"Auckland\"}"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ] {
            chunks.extend(parser.handle_data(data));
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
        assert_eq!(args_for("call_a"), r#"{"city":"Auckland"}"#);
        assert_eq!(args_for("call_b"), r#"{"tz":"Pacific/Auckland"}"#);
        assert!(chunks
            .iter()
            .filter_map(|c| c.tool_call_delta.as_ref())
            .all(|d| (d.call_id == "call_a" && d.tool_id == "get_weather")
                || (d.call_id == "call_b" && d.tool_id == "get_time")));
        assert_eq!(
            chunks.last().unwrap().finish_reason,
            Some(FinishReason::ToolCall)
        );
    }

    /// Servers that never send a tool-call `id` (seen in some local
    /// llama.cpp builds) still get distinct per-index call ids.
    #[test]
    fn parser_synthesizes_call_ids_when_server_omits_them() {
        let mut parser = OpenAiSseParser::new(ComputeMode::Hosted);
        let mut chunks = Vec::new();
        for data in [
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"alpha","arguments":"{}"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"name":"beta","arguments":"{}"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ] {
            chunks.extend(parser.handle_data(data));
        }
        let deltas: Vec<ToolCallDelta> = chunks
            .into_iter()
            .map(Result::unwrap)
            .filter_map(|c| c.tool_call_delta)
            .collect();
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas[0].call_id, "call_0");
        assert_eq!(deltas[1].call_id, "call_1");
        assert_ne!(deltas[0].call_id, deltas[1].call_id);
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
        let events = stream::iter(vec![Ok::<_, Infallible>(raw.as_bytes())]).eventsource();
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
        assert_eq!(
            chunks.last().unwrap().finish_reason,
            Some(FinishReason::Stop)
        );
        assert!(chunks.iter().all(|c| c.active_mode == ComputeMode::Hosted));
    }

    /// End-to-end: canned tool-call SSE bytes -> eventsource -> parser.
    #[tokio::test]
    async fn sse_bytes_tool_call_end_to_end() {
        use crate::hosted::sse::chunks_from_events;
        use futures::{stream, StreamExt};
        use std::convert::Infallible;

        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{\"index\":0,\"id\":\"call_9\",\"type\":\"function\",\"function\":{\"name\":\"file_read\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"notes.txt\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let events = stream::iter(vec![Ok::<_, Infallible>(raw.as_bytes())]).eventsource();
        let chunks: Vec<_> = chunks_from_events(
            events,
            OpenAiSseParser::new(ComputeMode::Hosted),
            CancellationToken::new(),
        )
        .collect()
        .await;

        let chunks: Vec<_> = chunks.into_iter().map(Result::unwrap).collect();
        let args: String = chunks
            .iter()
            .filter_map(|c| c.tool_call_delta.as_ref())
            .map(|d| d.args_delta.as_str())
            .collect();
        assert_eq!(args, r#"{"path":"notes.txt"}"#);
        assert!(chunks
            .iter()
            .filter_map(|c| c.tool_call_delta.as_ref())
            .all(|d| d.call_id == "call_9" && d.tool_id == "file_read"));
        assert_eq!(
            chunks.last().unwrap().finish_reason,
            Some(FinishReason::ToolCall)
        );
    }
}
