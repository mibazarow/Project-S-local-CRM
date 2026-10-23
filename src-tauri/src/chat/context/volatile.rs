// The volatile [Context] block (SPEC 4.6): everything that changes between
// turns and must not sit in the stable system prompt. Assembled from its
// sections, then prepended to the last user turn of the model-facing history
// only — the stored row is never touched, so the system prompt and the older
// history stay byte-identical and the prompt cache keeps its prefix.
use crate::llm::{ChatMessage, ContentPart, MessageContent, Role};

// Header of the volatile block prepended to the fresh user turn.
pub const CONTEXT_BLOCK_PREFIX: &str = "[Context]";

// Assembles the volatile [Context] block from its sections (project brief,
// query-relevant memory records, notes vault summary): everything that
// changes between turns and must not sit in the stable system prompt. Empty
// sections are dropped; an all-empty input yields the empty string, which
// build_context then does not prepend at all.
pub fn volatile_block(sections: &[&str]) -> String {
    let body: Vec<&str> = sections
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if body.is_empty() {
        return String::new();
    }
    format!("{CONTEXT_BLOCK_PREFIX}\n{}\n[/Context]", body.join("\n\n"))
}

// Prepends the volatile block to the last user turn of the model-facing
// history, before the user's own text. The stored row is never touched: the
// block exists only in this run's transcript, so the system prompt and the
// older history stay byte-identical and the prompt cache keeps its prefix.
pub(crate) fn attach_volatile_block(history: &mut [ChatMessage], block: &str) {
    if block.is_empty() {
        return;
    }
    let Some(msg) = history.iter_mut().rev().find(|m| m.role == Role::User) else {
        return;
    };
    match &mut msg.content {
        Some(MessageContent::Text(text)) => {
            *text = format!("{block}\n\n{text}");
        }
        Some(MessageContent::Parts(parts)) => {
            match parts
                .iter_mut()
                .find(|p| matches!(p, ContentPart::Text { .. }))
            {
                Some(ContentPart::Text { text }) => *text = format!("{block}\n\n{text}"),
                // A parts turn without any text part gets a leading one.
                _ => parts.insert(0, ContentPart::text(block)),
            }
        }
        None => msg.content = Some(MessageContent::Text(block.to_string())),
    }
}
