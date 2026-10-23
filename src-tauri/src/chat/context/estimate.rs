// Deterministic token estimation for the compaction trigger (SPEC 4.6): a
// cheap, Cyrillic-aware heuristic, not a tokenizer. Image content parts carry a
// fixed token allowance instead of their base64 length, which would wildly
// inflate the estimate.
use crate::llm::{ChatMessage, ContentPart, MessageContent, Role};

// Cyrillic letters live in the Cyrillic block plus its supplement (А-я, Ё-ё
// and the extended historic letters).
fn is_cyrillic(c: char) -> bool {
    matches!(c, '\u{0400}'..='\u{04FF}' | '\u{0500}'..='\u{052F}')
}

// Cyrillic-aware text estimate: Cyrillic tokenizes denser than Latin, at
// roughly 2 chars per token against ~4 for everything else. Counting the two
// classes separately keeps Russian transcripts from being underestimated.
pub fn estimate_text_tokens(text: &str) -> usize {
    let mut cyrillic = 0usize;
    let mut other = 0usize;
    for c in text.chars() {
        if is_cyrillic(c) {
            cyrillic += 1;
        } else {
            other += 1;
        }
    }
    cyrillic / 2 + other / 4
}

// Heuristic token estimate: the per-text estimate above plus a per-message
// framing overhead. Deterministic and cheap; it is an estimate, not a
// tokenizer. Image content parts are not counted by their base64 length
// (that would wildly inflate the estimate); each image carries a fixed
// token allowance.
pub fn estimate_tokens(messages: &[ChatMessage]) -> usize {
    const PER_MESSAGE: usize = 8;
    const PER_IMAGE_TOKENS: usize = 1000;
    messages
        .iter()
        .map(|m| {
            let (chars, images) = match &m.content {
                Some(MessageContent::Text(c)) => (estimate_text_tokens(c), 0),
                Some(MessageContent::Parts(parts)) => {
                    let chars = parts
                        .iter()
                        .map(|p| match p {
                            ContentPart::Text { text } => estimate_text_tokens(text),
                            ContentPart::ImageUrl { .. } => 0,
                        })
                        .sum();
                    let images = parts
                        .iter()
                        .filter(|p| matches!(p, ContentPart::ImageUrl { .. }))
                        .count();
                    (chars, images)
                }
                None => (0, 0),
            };
            chars + images * PER_IMAGE_TOKENS
                + if m.role == Role::Assistant {
                    m.reasoning_content
                        .as_deref()
                        .map(estimate_text_tokens)
                        .unwrap_or(0)
                } else {
                    0
                }
                + m.tool_calls
                    .iter()
                    .map(|tc| {
                        estimate_text_tokens(&tc.name) + estimate_text_tokens(&tc.raw_arguments)
                    })
                    .sum::<usize>()
                + PER_MESSAGE
        })
        .sum()
}
