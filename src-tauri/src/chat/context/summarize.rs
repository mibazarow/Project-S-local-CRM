// The summarizer itself (SPEC 4.6, 6.4): flattening the expired range into a
// plain-text transcript (also reused by the memory FactExtractor), the char cap
// that keeps the compaction call inside the window it is protecting, and the
// LLM call that writes the checkpoint summary.
use crate::agent::r#loop::ChatBackend;
use crate::error::{AppError, Result};
use crate::llm::{ChatMessage, ChatParams, ContentPart, MessageContent, Role};

// Cap on what the summarizer itself receives, so the compaction call cannot
// overflow the window it is trying to protect.
pub(crate) const SUMMARIZER_MAX_CHARS: usize = 60_000;

// A summarizer request must itself fit the model window. The transcript is
// capped to roughly one character per token (Cyrillic is ~2 chars/token), so a
// small window does not turn compaction into another over-limit request.
pub(crate) fn summarizer_char_cap(context_window: u64) -> usize {
    (context_window as usize).clamp(1_000, SUMMARIZER_MAX_CHARS)
}

// Flattens messages into plain text for the summarizer prompt (also used by
// the memory FactExtractor: same transcript, caller-supplied char cap).
pub(crate) fn render_transcript_capped(messages: &[ChatMessage], max_chars: usize) -> String {
    let mut out = String::new();
    for m in messages {
        let role = match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        match &m.content {
            Some(MessageContent::Text(content)) => out.push_str(&format!("{role}: {content}\n")),
            Some(MessageContent::Parts(parts)) => {
                let text: Vec<&str> = parts
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.as_str()),
                        ContentPart::ImageUrl { .. } => None,
                    })
                    .collect();
                let has_image = parts.iter().any(|p| matches!(p, ContentPart::ImageUrl { .. }));
                let suffix = if has_image { " (image)" } else { "" };
                out.push_str(&format!("{role}: {}{suffix}\n", text.join(" ")));
            }
            None => {}
        }
        for tc in &m.tool_calls {
            out.push_str(&format!("assistant[tool_call {}]: {}\n", tc.name, tc.raw_arguments));
        }
    }
    const SKIPPED: &str = "\n[…part of the conversation skipped…]\n";
    if out.chars().count() > max_chars {
        let keep = max_chars.saturating_sub(SKIPPED.chars().count()) / 2;
        let head: String = out.chars().take(keep).collect();
        let tail: String = out.chars().skip(out.chars().count() - keep).collect();
        out = format!("{head}{SKIPPED}{tail}");
    }
    out
}

pub(crate) fn summarize(
    backend: &dyn ChatBackend,
    params: &ChatParams,
    transcript: &str,
) -> Result<String> {
    let messages = vec![
        ChatMessage::system(
            "You are a summarizer of an agent conversation. Compress the given part of the \
conversation into a brief coherent summary in Russian: keep the decisions, important facts, tool \
calls and their results, errors, and open questions. Do not answer the user, write only the summary.",
        ),
        ChatMessage::user(transcript),
    ];
    let outcome = backend.chat_stream(&messages, None, params, &mut |_| {})?;
    let summary = outcome.content.unwrap_or_default();
    if summary.trim().is_empty() {
        return Err(AppError::Invalid("summarizer returned an empty summary".into()));
    }
    Ok(summary.trim().to_string())
}
