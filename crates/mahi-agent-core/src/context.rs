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

    if let Some(prompt) = &conversation.system_prompt {
        preamble.push(synthetic_system(conversation, prompt.clone()));
    }

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
