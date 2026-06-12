//! Context assembly: system prompt + recalled memory + recent history, with a
//! simple sliding-window compaction step that keeps the context inside a
//! character budget (a cheap proxy for tokens in Phase 0).
//!
//! TODO(contracts): hierarchical summarization (domain doc §5.3) is deferred;
//! the sliding window here is the on-device/Phase-0 strategy.

use mahi_contracts::data::{Conversation, DataStore, Message, MessageRole};
use mahi_contracts::error::ContractError;

/// Character budget for the assembled context (~4k tokens at 4 chars/token).
pub(crate) const CONTEXT_CHAR_BUDGET: usize = 16_000;

/// The default system prompt used when a conversation doesn't set its own.
///
/// Shapes Mahi into a capable, Claude-style local coworker: proactive,
/// tool-using, computer-using, safe, and concise. It deliberately doesn't
/// enumerate tools (those arrive via the request's tool specs) — it gives the
/// behavioural frame that small local models especially need to act like an
/// agent rather than a chatbot.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are Mahi, a capable personal AI assistant running on the user's own Mac. \
You act as a proactive coworker: understand the goal, make a short plan, and carry it out end to end rather than just describing what could be done.\n\n\
You have tools available this turn — reading, writing, editing and searching files; running shell commands; fetching and searching the web; \
and, when working on the Mac, controlling the computer (capturing the screen, reading the on-screen UI, clicking, typing, scrolling, and pressing keys). \
Use them whenever they help, and prefer acting over asking. Call one or more tools, look at the results, and keep going until the task is actually done. \
When you write or change code, match the surrounding style and keep edits minimal, correct, and runnable.\n\n\
For computer use: capture or describe the screen before you act so you target the right element, take one careful step at a time, and check the result before the next step. \
Some actions require the user's approval — when one is requested, wait for the decision and respect it. \
Never type credentials or act on login, password, or payment screens; stop and let the user handle those directly.\n\n\
Be concise and direct, and explain your actions only as much as is useful. Break large tasks into steps, and delegate independent sub-tasks to parallel subagents when that's faster. \
If a request is ambiguous or risky, say so briefly and proceed with the most reasonable interpretation.";

/// Build the message list for an inference round: synthetic system prompt,
/// synthetic memory-recall message, then compacted recent history.
pub(crate) async fn assemble_context(
    data: &DataStore,
    conversation: &Conversation,
    recall_query: &str,
    history_limit: usize,
    memory_limit: usize,
    budget_chars: usize,
) -> Result<Vec<Message>, ContractError> {
    let mut preamble: Vec<Message> = Vec::new();

    // Always give the model a system prompt: the conversation's own, or the
    // capable default, so every surface behaves like an agent out of the box.
    let system_prompt = conversation
        .system_prompt
        .clone()
        .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());
    preamble.push(synthetic_system(conversation, system_prompt));

    let memories = data
        .memory
        .semantic_search(recall_query, conversation.space_id, memory_limit)
        .await?;
    if !memories.is_empty() {
        let lines: Vec<String> = memories
            .iter()
            .map(|m| format!("- {}", m.content))
            .collect();
        preamble.push(synthetic_system(
            conversation,
            format!(
                "Relevant memories recalled for this turn:\n{}",
                lines.join("\n")
            ),
        ));
    }

    let history = data.messages.range(conversation.id, history_limit).await?;
    Ok(compact(preamble, history, budget_chars))
}

/// A synthetic (never persisted) system message; `sequence_num` is -1 to mark
/// it as out-of-band.
fn synthetic_system(conversation: &Conversation, text: String) -> Message {
    Message::text(
        conversation.id,
        MessageRole::System,
        text,
        conversation.mode_at_creation,
        -1,
    )
}

/// Sliding-window compaction: always keep the preamble and the most recent
/// history message, then walk backwards keeping messages while they fit in
/// `budget_chars`.
pub(crate) fn compact(
    preamble: Vec<Message>,
    history: Vec<Message>,
    budget_chars: usize,
) -> Vec<Message> {
    let mut used: usize = preamble.iter().map(message_cost).sum();
    let mut kept: Vec<Message> = Vec::new();
    for msg in history.into_iter().rev() {
        let cost = message_cost(&msg);
        if !kept.is_empty() && used + cost > budget_chars {
            break;
        }
        used += cost;
        kept.push(msg);
    }
    kept.reverse();

    let mut out = preamble;
    out.extend(kept);
    out
}

/// Approximate cost of a message: length of its serialized content blocks.
fn message_cost(msg: &Message) -> usize {
    serde_json::to_string(&msg.content)
        .map(|s| s.len())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mahi_contracts::types::ComputeMode;
    use uuid::Uuid;

    fn msg(conv: Uuid, seq: i64, text: &str) -> Message {
        Message::text(conv, MessageRole::User, text, ComputeMode::OnDevice, seq)
    }

    #[test]
    fn compact_keeps_most_recent_within_budget() {
        let conv = Uuid::new_v4();
        let history: Vec<Message> = (0..10)
            .map(|i| msg(conv, i, &format!("message number {i} {}", "x".repeat(100))))
            .collect();
        let compacted = compact(Vec::new(), history, 400);
        assert!(!compacted.is_empty());
        assert!(compacted.len() < 10, "should have dropped old messages");
        // The newest message must survive compaction.
        assert_eq!(compacted.last().unwrap().sequence_num, 9);
        // Order is preserved oldest → newest.
        let seqs: Vec<i64> = compacted.iter().map(|m| m.sequence_num).collect();
        let mut sorted = seqs.clone();
        sorted.sort();
        assert_eq!(seqs, sorted);
    }

    #[test]
    fn compact_always_keeps_latest_even_over_budget() {
        let conv = Uuid::new_v4();
        let history = vec![msg(conv, 0, &"y".repeat(5000))];
        let compacted = compact(Vec::new(), history, 10);
        assert_eq!(compacted.len(), 1);
    }

    #[tokio::test]
    async fn injects_default_system_prompt_when_conversation_has_none() {
        let data = mahi_contracts::testkit::in_memory_datastore();
        let conv = Conversation::new(ComputeMode::OnDevice);
        assert!(conv.system_prompt.is_none());
        let ctx = assemble_context(&data, &conv, "hello", 50, 5, CONTEXT_CHAR_BUDGET)
            .await
            .unwrap();
        assert_eq!(ctx[0].role, MessageRole::System);
        let text = ctx[0].text_content();
        assert!(text.contains("Mahi"), "default prompt names the assistant");
        assert!(text.contains("tools"), "default prompt frames tool use");
    }

    #[tokio::test]
    async fn conversation_system_prompt_overrides_the_default() {
        let data = mahi_contracts::testkit::in_memory_datastore();
        let mut conv = Conversation::new(ComputeMode::OnDevice);
        conv.system_prompt = Some("You are a terse poet.".to_string());
        let ctx = assemble_context(&data, &conv, "hello", 50, 5, CONTEXT_CHAR_BUDGET)
            .await
            .unwrap();
        assert_eq!(ctx[0].role, MessageRole::System);
        assert_eq!(ctx[0].text_content(), "You are a terse poet.");
    }

    #[test]
    fn compact_preserves_preamble() {
        let conv = Uuid::new_v4();
        let preamble = vec![msg(conv, -1, "system prompt")];
        let history = vec![msg(conv, 0, "hi"), msg(conv, 1, "there")];
        let compacted = compact(preamble, history, CONTEXT_CHAR_BUDGET);
        assert_eq!(compacted.len(), 3);
        assert_eq!(compacted[0].sequence_num, -1);
    }
}
