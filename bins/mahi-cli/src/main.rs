//! Mahi CLI — an interactive REPL that drives the full agent stack:
//! the [`MahiEngine`] over a real data store, the inference router (on-device
//! mock by default, hosted providers behind the `hosted` feature), and the
//! built-in tool registry (file/shell tools + Claude-style computer use).

use anyhow::{anyhow, Result};
use clap::Parser;
use futures::StreamExt;
use mahi_agent_core::{EngineConfig, MahiEngine};
use mahi_compute::{InferenceRouter, OnDeviceProvider};
use mahi_contracts::agent::AgentEvent;
use mahi_contracts::compute::InferenceProvider;
use mahi_contracts::tooling::{ToolEvent, ToolInvokeContract};
use mahi_contracts::types::ComputeMode;
use mahi_tooling::{MockComputerController, ToolRegistry};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Mahi AI — local-first assistant CLI.
#[derive(Parser)]
#[command(name = "mahi", version, about)]
struct Cli {
    /// Store location: "memory" (ephemeral) or a directory path (encrypted on disk).
    #[arg(long, default_value = "memory")]
    store: String,
    /// Model id for hosted providers.
    #[arg(long, default_value = "claude-fable-5")]
    model: String,
    /// Force the on-device mock model even when API keys are present.
    #[arg(long)]
    mock: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    #[cfg(not(feature = "hosted"))]
    let _ = (&cli.model, cli.mock);

    // ── Data store ──────────────────────────────────────────────────────────
    let data = if cli.store == "memory" {
        mahi_data::open_in_memory()
    } else {
        mahi_data::open_store(&PathBuf::from(&cli.store))
    }
    .map_err(|e| anyhow!("opening store: {e}"))?;

    // ── Inference: on-device always; hosted if a key is present ─────────────
    #[allow(unused_mut)]
    let mut builder = InferenceRouter::builder()
        .add_provider(ComputeMode::OnDevice, Arc::new(OnDeviceProvider::new()));
    #[allow(unused_mut)]
    let mut active = "on-device (mock model)".to_string();
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
            active = format!("hosted · anthropic/{}", cli.model);
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
            active = format!("hosted · openai-compatible/{}", cli.model);
        }
    }
    let inference: Arc<dyn InferenceProvider> = Arc::new(builder.build());

    // ── Tools: file ops scoped to the CWD; computer-use mocked off-Mac ──────
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let tools_handle = Arc::new(ToolRegistry::with_builtins_scoped(
        Arc::new(MockComputerController::new()),
        &cwd,
    ));
    let tools: Arc<dyn ToolInvokeContract> = tools_handle.clone();

    // ── Engine ──────────────────────────────────────────────────────────────
    let engine = MahiEngine::new(EngineConfig {
        data,
        inference,
        tools,
        device_id: Uuid::new_v4(),
    });
    let mut conversation = engine
        .create_conversation(ComputeMode::OnDevice)
        .await
        .map_err(|e| anyhow!("creating conversation: {e}"))?;

    let mut out = tokio::io::stdout();
    let mut reader = BufReader::new(tokio::io::stdin());
    out.write_all(
        format!("Mahi CLI · {active}\nType a message, or /help for commands.\n\n").as_bytes(),
    )
    .await?;

    let mut line = String::new();
    loop {
        out.write_all(b"you > ").await?;
        out.flush().await?;
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            out.write_all(b"\n").await?;
            break; // EOF (Ctrl-D)
        }
        let input = line.trim().to_string();
        if input.is_empty() {
            continue;
        }
        if input == "/quit" || input == "/exit" {
            break;
        } else if input == "/help" {
            out.write_all(
                b"commands:\n  /new            start a fresh conversation\n  /tools          list available tools\n  /spawn a | b    run parallel subagents\n  /quit           exit\n",
            )
            .await?;
            continue;
        } else if input == "/new" {
            conversation = engine
                .create_conversation(ComputeMode::OnDevice)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            out.write_all(b"(new conversation)\n").await?;
            continue;
        } else if input == "/tools" {
            for d in tools_handle.describe(ComputeMode::MacLan).await {
                let flag = if d.requires_approval {
                    " (needs approval)"
                } else {
                    ""
                };
                out.write_all(format!("  {:<16} {}{}\n", d.id, d.display_name, flag).as_bytes())
                    .await?;
            }
            continue;
        } else if let Some(rest) = input.strip_prefix("/spawn ") {
            let goals: Vec<String> = rest
                .split('|')
                .map(|g| g.trim().to_string())
                .filter(|g| !g.is_empty())
                .collect();
            if goals.is_empty() {
                out.write_all(b"usage: /spawn goal one | goal two\n")
                    .await?;
                continue;
            }
            out.write_all(format!("spawning {} subagents…\n", goals.len()).as_bytes())
                .await?;
            match engine.spawn_subagents(goals).await {
                Ok(results) => {
                    for (i, r) in results.iter().enumerate() {
                        out.write_all(format!("  subagent {}: {}\n", i + 1, brief(r)).as_bytes())
                            .await?;
                    }
                }
                Err(e) => {
                    out.write_all(format!("  subagent error: {e}\n").as_bytes())
                        .await?
                }
            }
            continue;
        }

        // A normal chat turn.
        let cancel = CancellationToken::new();
        let mut stream = match engine.run_turn(conversation, input, cancel).await {
            Ok(s) => s,
            Err(e) => {
                out.write_all(format!("[error] {e}\n").as_bytes()).await?;
                continue;
            }
        };
        out.write_all(b"mahi > ").await?;
        out.flush().await?;
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(AgentEvent::TextDelta { text }) => {
                    out.write_all(text.as_bytes()).await?;
                    out.flush().await?;
                }
                Ok(AgentEvent::Tool { event }) => {
                    if let Some(s) = render_tool_event(&event) {
                        out.write_all(s.as_bytes()).await?;
                        out.flush().await?;
                    }
                }
                Ok(AgentEvent::ApprovalRequired {
                    approval_id,
                    summary,
                }) => {
                    out.write_all(format!("\n[approval] {summary}\napprove? [y/N] ").as_bytes())
                        .await?;
                    out.flush().await?;
                    line.clear();
                    reader.read_line(&mut line).await?;
                    let approved = matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes");
                    engine
                        .resolve_approval(approval_id, approved)
                        .await
                        .map_err(|e| anyhow!("{e}"))?;
                    out.write_all(if approved {
                        b"mahi > "
                    } else {
                        b"(denied) mahi > "
                    })
                    .await?;
                    out.flush().await?;
                }
                Ok(AgentEvent::TurnFinished { .. }) => {
                    out.write_all(b"\n").await?;
                }
                Ok(AgentEvent::Error { message }) => {
                    out.write_all(format!("\n[error] {message}\n").as_bytes())
                        .await?;
                }
                Ok(AgentEvent::TurnStarted { .. }) | Ok(AgentEvent::ModeHandoff { .. }) => {}
                Err(e) => {
                    out.write_all(format!("\n[stream error] {e}\n").as_bytes())
                        .await?;
                    break;
                }
            }
        }
    }
    out.write_all(b"bye.\n").await?;
    out.flush().await?;
    Ok(())
}

/// A one-line, length-capped summary of a tool event (or `None` to suppress).
fn render_tool_event(event: &ToolEvent) -> Option<String> {
    match event {
        ToolEvent::Result { output, .. } => Some(format!(
            "\n  ⮑ tool result: {}\n",
            brief(&output.to_string())
        )),
        ToolEvent::Error { message, .. } => Some(format!("\n  ⮑ tool error: {message}\n")),
        ToolEvent::Citation { url, .. } => Some(format!("\n  ⮑ source: {url}\n")),
        ToolEvent::ApprovalRequired { summary, .. } => Some(format!("\n  ⮑ {summary}\n")),
        ToolEvent::Cancelled => Some("\n  ⮑ tool cancelled\n".to_string()),
        ToolEvent::Chunk { .. } => None,
    }
}

/// Truncate a string to a readable length for the terminal.
fn brief(s: &str) -> String {
    const MAX: usize = 200;
    let s = s.trim();
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(MAX).collect();
        out.push('…');
        out
    }
}
