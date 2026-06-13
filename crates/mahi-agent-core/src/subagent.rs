//! Subagent coordination: fan a list of goals out to isolated sub-engines run
//! in parallel, and collect each subagent's final assistant text.
//!
//! Each subagent gets a fresh conversation and a fresh `MahiEngine` (isolated
//! approval registry, isolated context) while sharing the parent's data store,
//! inference provider, and tool registry.
//!
//! TODO(contracts): subagent approvals are not yet routed to the parent
//! surface — Phase 0 subagents should be given non-approval tool sets only
//! (see domain doc §5.6, in-process isolation + budgets first).

use crate::engine::{EngineConfig, MahiEngine};
use futures::future::join_all;
use futures::StreamExt;
use mahi_contracts::agent::AgentEvent;
use mahi_contracts::data::MessageRole;
use mahi_contracts::error::ContractError;
use mahi_contracts::types::ComputeMode;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Most subagents that may execute concurrently. The fan-out is genuinely
/// parallel, but the shared inference backend (a single local `llama-server`,
/// or a rate-limited hosted API) and the single-writer store make unbounded
/// parallelism counter-productive — so we cap concurrency and let the rest
/// queue. Ten matches the product cap ("up to 10 agents at a time").
pub const MAX_CONCURRENT_SUBAGENTS: usize = 10;

/// Spawns isolated sub-`MahiEngine` turns for a list of goals and gathers
/// their summaries.
pub struct SubagentCoordinator {
    config: EngineConfig,
    max_concurrent: usize,
}

impl SubagentCoordinator {
    /// Build a coordinator from the seams a sub-engine should share.
    pub fn new(config: EngineConfig) -> Self {
        Self {
            config,
            max_concurrent: MAX_CONCURRENT_SUBAGENTS,
        }
    }

    /// Build a coordinator that shares `engine`'s data/inference/tools.
    pub fn from_engine(engine: &MahiEngine) -> Self {
        Self::new(engine.config_clone())
    }

    /// Override the concurrency cap (clamped to at least 1 and at most
    /// [`MAX_CONCURRENT_SUBAGENTS`]).
    pub fn with_max_concurrent(mut self, max: usize) -> Self {
        self.max_concurrent = max.clamp(1, MAX_CONCURRENT_SUBAGENTS);
        self
    }

    /// Run every goal as its own single-turn subagent and return the final
    /// assistant text of each (in goal order). Up to `max_concurrent` run at
    /// once; the rest queue on a semaphore so a large batch can't swamp the
    /// shared model/store.
    pub async fn run_goals(
        &self,
        goals: Vec<String>,
        mode: ComputeMode,
    ) -> Result<Vec<String>, ContractError> {
        let permits = Arc::new(Semaphore::new(self.max_concurrent));
        let turns = goals.into_iter().map(|goal| {
            let engine = MahiEngine::new(self.config.clone());
            let permits = Arc::clone(&permits);
            async move {
                // Hold a permit for the whole subagent turn; dropped on finish.
                let _permit = permits
                    .acquire()
                    .await
                    .expect("subagent semaphore is never closed");
                run_one_subagent(engine, goal, mode).await
            }
        });
        join_all(turns).await.into_iter().collect()
    }
}

/// One subagent: fresh conversation → one turn → final assistant text.
async fn run_one_subagent(
    engine: MahiEngine,
    goal: String,
    mode: ComputeMode,
) -> Result<String, ContractError> {
    let conversation_id = engine.create_conversation(mode).await?;
    let mut events = engine
        .run_turn(conversation_id, goal, CancellationToken::new())
        .await?;

    // Drain the stream; keep the streamed text as a fallback summary.
    let mut streamed_text = String::new();
    while let Some(item) = events.next().await {
        match item? {
            AgentEvent::TextDelta { text } => streamed_text.push_str(&text),
            AgentEvent::TurnFinished { .. } => break,
            _ => {}
        }
    }

    // Prefer the persisted final assistant message (streamed text can include
    // intermediate segments between tool calls).
    let history = engine.history(conversation_id).await?;
    let summary = history
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Assistant)
        .map(|m| m.text_content())
        .unwrap_or(streamed_text);
    Ok(summary)
}
