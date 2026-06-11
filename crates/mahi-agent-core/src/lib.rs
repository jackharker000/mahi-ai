//! mahi-agent-core — the runtime brain: agent loop, sessions, context, approval gate.
//!
//! The public API is normative and defined by
//! `docs/backend/phase-0/03-engine-facade.md` (§"mahi-agent-core public API").
//! Detailed domain design: `docs/backend/domains/01-agent-core.md`.
//!
//! Entry point is [`MahiEngine`], constructed from an [`EngineConfig`] that
//! injects the three seams from `mahi-contracts`:
//!
//! - `DataStore` for persistence (conversations, messages, memory, audit, ...)
//! - `InferenceProvider` for model generation (typically the compute router)
//! - `ToolInvokeContract` for tool discovery and invocation
//!
//! [`MahiEngine::run_turn`] drives the streaming think→act→observe loop and
//! returns an [`AgentEventStream`]. Destructive tools are gated through
//! [`MahiEngine::resolve_approval`]. Parallel delegation is available via
//! [`MahiEngine::spawn_subagents`] / [`SubagentCoordinator`].

mod approval;
mod context;
mod engine;
mod subagent;

pub use engine::{AgentEventStream, EngineConfig, MahiEngine};
pub use subagent::SubagentCoordinator;
