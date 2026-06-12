//! Mahi home-server daemon.
//!
//! Exposes the inference router over an OpenAI-compatible HTTP API so other
//! devices (the phone in modes B/C) can use this machine's brain:
//!
//! - `GET  /health`               → liveness
//! - `GET  /v1/models`            → the active model descriptor
//! - `POST /v1/chat/completions`  → streaming (SSE) or buffered completion
//!
//! On-device mock model by default; hosted providers behind the `hosted`
//! feature when `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` is set.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use futures::StreamExt;
use mahi_compute::{InferenceRouter, OnDeviceProvider};
use mahi_contracts::compute::{InferenceProvider, InferenceRequest};
use mahi_contracts::data::{Message, MessageRole};
use mahi_contracts::types::ComputeMode;
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Mahi home-server daemon.
#[derive(Parser)]
#[command(name = "mahi-daemon", version, about)]
struct Cli {
    /// Port to listen on.
    #[arg(long, default_value_t = 11434)]
    port: u16,
    /// Model id advertised / used for hosted providers.
    #[arg(long, default_value = "claude-fable-5")]
    model: String,
    /// Force the on-device mock model even when API keys are present.
    #[arg(long)]
    mock: bool,
}

struct AppState {
    router: InferenceRouter,
    model: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    #[cfg(not(feature = "hosted"))]
    let _ = cli.mock;

    #[allow(unused_mut)]
    let mut builder = InferenceRouter::builder()
        .add_provider(ComputeMode::OnDevice, Arc::new(OnDeviceProvider::new()));
    #[allow(unused_mut)]
    let mut model_label = "mahi-on-device".to_string();
    #[cfg(feature = "hosted")]
    if !cli.mock {
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            builder = builder.add_provider(
                ComputeMode::Hosted,
                Arc::new(mahi_compute::AnthropicProvider::with_model(
                    key,
                    cli.model.clone(),
                )),
            );
            model_label = cli.model.clone();
        } else if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            let base = std::env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com".to_string());
            builder = builder.add_provider(
                ComputeMode::Hosted,
                Arc::new(mahi_compute::OpenAiCompatProvider::new(
                    base,
                    key,
                    cli.model.clone(),
                )),
            );
            model_label = cli.model.clone();
        }
    }

    let state = Arc::new(AppState {
        router: builder.build(),
        model: model_label,
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state);

    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, cli.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("mahi-daemon listening on http://{addr}");
    println!("mahi-daemon listening on http://{addr} (OpenAI-compatible)");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn health() -> &'static str {
    "ok"
}

async fn models(State(state): State<Arc<AppState>>) -> Response {
    let d = state.router.descriptor();
    Json(json!({
        "object": "list",
        "data": [{
            "id": state.model,
            "object": "model",
            "owned_by": "mahi",
            "context_window": d.context_window,
        }],
    }))
    .into_response()
}

#[derive(Deserialize)]
struct ChatRequest {
    #[serde(default)]
    messages: Vec<ChatMessage>,
    #[serde(default)]
    stream: bool,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct ChatMessage {
    role: String,
    content: String,
}

fn to_request(req: &ChatRequest) -> InferenceRequest {
    let conversation = Uuid::nil();
    let messages: Vec<Message> = req
        .messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let role = match m.role.as_str() {
                "assistant" => MessageRole::Assistant,
                "system" => MessageRole::System,
                "tool" => MessageRole::Tool,
                _ => MessageRole::User,
            };
            Message::text(
                conversation,
                role,
                m.content.clone(),
                ComputeMode::OnDevice,
                i as i64,
            )
        })
        .collect();
    let mut ir = InferenceRequest::from_messages(messages);
    ir.temperature = req.temperature;
    ir.max_tokens = req.max_tokens;
    ir
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> Response {
    let infer = to_request(&req);
    let model = state.model.clone();

    let stream = match state.router.generate(infer, CancellationToken::new()).await {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    };

    if req.stream {
        let model_c = model.clone();
        let body = stream
            .filter_map(move |res| {
                let model = model_c.clone();
                async move {
                    let chunk = res.ok()?;
                    let content = chunk.delta?;
                    let payload = json!({
                        "object": "chat.completion.chunk",
                        "model": model,
                        "choices": [{ "index": 0, "delta": { "content": content }, "finish_reason": null }],
                    });
                    Some(Ok::<Event, Infallible>(Event::default().data(payload.to_string())))
                }
            })
            .chain(futures::stream::once(async {
                Ok(Event::default().data("[DONE]"))
            }));
        Sse::new(body).into_response()
    } else {
        let mut content = String::new();
        let mut stream = stream;
        while let Some(res) = stream.next().await {
            if let Ok(chunk) = res {
                if let Some(d) = chunk.delta {
                    content.push_str(&d);
                }
            }
        }
        Json(json!({
            "object": "chat.completion",
            "model": model,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": "stop",
            }],
        }))
        .into_response()
    }
}
