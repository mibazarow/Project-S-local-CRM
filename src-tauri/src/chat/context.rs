// Context compaction for the project agent (SPEC 4.6 "автосуммаризация при
// 80% окна", SPEC 6.4 "Сжатие контекста с чекпоинтами"). When the estimated
// context crosses the threshold of the model window, the oldest messages are
// folded into checkpoint summaries stored in `summaries`; the raw transcript
// stays append-only and only the model-facing view is compacted, so the
// untouched prefix stays prompt-cache friendly. On any summarization failure
// the uncompacted history is returned: compaction must never break a chat.
use super::load_history;
use crate::agent::r#loop::ChatBackend;
use crate::error::{AppError, Result};
use crate::llm::{ChatMessage, ChatParams, ContentPart, MessageContent, Role};
use rusqlite::{params, Connection};

// Compaction trigger defaults to 80% of the model context window (SPEC 4.6);
// the value lives in app.json `compactionThresholdPercent` (AppConfig).
// Header of the synthetic message that replaces a summarized range.
pub const SUMMARY_PREFIX: &str = "[Summary of the earlier conversation]";

// Cap on what the summarizer itself receives, so the compaction call cannot
// overflow the window it is trying to protect.
const SUMMARIZER_MAX_CHARS: usize = 60_000;

// Heuristic token estimate: ~4 chars per token plus a per-message framing
// overhead. Deterministic and cheap; it is an estimate, not a tokenizer.
// Image content parts are not counted by their base64 length (that would
// wildly inflate the estimate); each image carries a fixed token allowance.
pub fn estimate_tokens(messages: &[ChatMessage]) -> usize {
    const PER_MESSAGE: usize = 8;
    const PER_IMAGE_TOKENS: usize = 1000;
    messages
        .iter()
        .map(|m| {
            let (chars, images) = match &m.content {
                Some(MessageContent::Text(c)) => (c.chars().count(), 0),
                Some(MessageContent::Parts(parts)) => {
                    let chars = parts
                        .iter()
                        .map(|p| match p {
                            ContentPart::Text { text } => text.chars().count(),
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
            chars / 4 + images * PER_IMAGE_TOKENS
                + m.tool_calls
                    .iter()
                    .map(|tc| tc.name.chars().count() + tc.raw_arguments.chars().count())
                    .sum::<usize>()
                + PER_MESSAGE
        })
        .sum()
}

pub struct ContextOutcome {
    pub messages: Vec<ChatMessage>,
    // Set when this call wrote a new checkpoint row (for the UI event).
    pub new_checkpoint_up_to: Option<i64>,
}

// Builds the model-facing transcript: system prompt + checkpoint summaries +
// the recent tail, compacting the oldest messages when the estimate crosses
// `threshold_percent` of `context_window`. `chat_id` must already contain the
// fresh user turn: the last message is never dropped.
#[allow(clippy::too_many_arguments)]
pub fn build_context(
    conn: &Connection,
    chat_id: i64,
    system_prompt: &str,
    context_window: u64,
    threshold_percent: u8,
    backend: &dyn ChatBackend,
    params: &ChatParams,
    vision_supported: bool,
) -> Result<ContextOutcome> {
    let history = load_history(conn, chat_id, vision_supported)?;
    let system = ChatMessage::system(system_prompt);
    let mut full = Vec::with_capacity(history.len() + 1);
    full.push(system.clone());
    full.extend(history.iter().cloned());

    let threshold_tokens =
        (context_window as usize).saturating_mul(threshold_percent as usize) / 100;
    if estimate_tokens(&full) <= threshold_tokens {
        return Ok(ContextOutcome {
            messages: full,
            new_checkpoint_up_to: None,
        });
    }

    // The tail keeps its own slice of the window (window - threshold, i.e.
    // ~20% at the 80% default) so the compacted context fits back inside.
    let tail_budget = (context_window as usize).saturating_sub(threshold_tokens);
    let budget_boundary = pick_boundary(&history, tail_budget);
    // messages is append-only with contiguous positions 1..=N, so a message's
    // index in `history` equals its position minus one.
    let last_up_to: i64 = conn.query_row(
        "SELECT COALESCE(MAX(up_to_position), 0) FROM summaries WHERE chat_id = ?1",
        params![chat_id],
        |r| r.get(0),
    )?;
    let boundary = budget_boundary.max(last_up_to as usize);
    if boundary == 0 || boundary >= history.len() {
        return Ok(ContextOutcome {
            messages: full,
            new_checkpoint_up_to: None,
        });
    }

    let mut messages = vec![system];
    for summary in load_summaries(conn, chat_id)? {
        messages.push(ChatMessage::assistant(format!("{SUMMARY_PREFIX}\n{summary}")));
    }
    let mut new_checkpoint_up_to = None;
    if boundary as i64 > last_up_to {
        // Only the range beyond the last checkpoint is summarized; stored
        // checkpoints are reused, never re-summarized.
        let transcript = render_transcript(&history[last_up_to as usize..boundary]);
        match summarize(backend, params, &transcript) {
            Ok(summary) => {
                conn.execute(
                    "INSERT INTO summaries (chat_id, up_to_position, summary, created_at)
                     VALUES (?1, ?2, ?3, datetime('now','localtime'))",
                    params![chat_id, boundary as i64, &summary],
                )?;
                messages.push(ChatMessage::assistant(format!("{SUMMARY_PREFIX}\n{summary}")));
                new_checkpoint_up_to = Some(boundary as i64);
            }
            // LLM failure: fall back to the uncompacted history, chat goes on.
            Err(_) => {
                return Ok(ContextOutcome {
                    messages: full,
                    new_checkpoint_up_to: None,
                })
            }
        }
    }
    messages.extend(history[boundary..].iter().cloned());
    Ok(ContextOutcome {
        messages,
        new_checkpoint_up_to,
    })
}

// Index of the first history message kept verbatim; 0 means "nothing to
// compact". The last message (the fresh user turn) is always kept even if it
// alone exceeds the budget, and the tail never starts on an orphaned tool
// result: the boundary is pulled back to the assistant turn that issued it.
fn pick_boundary(history: &[ChatMessage], tail_budget: usize) -> usize {
    let mut tokens = 0usize;
    let mut boundary = history.len();
    for i in (0..history.len()).rev() {
        let cost = estimate_tokens(&history[i..i + 1]);
        if boundary < history.len() && tokens + cost > tail_budget {
            break;
        }
        tokens += cost;
        boundary = i;
    }
    while boundary > 0 && history[boundary].role == Role::Tool {
        boundary -= 1;
    }
    boundary
}

fn load_summaries(conn: &Connection, chat_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT summary FROM summaries WHERE chat_id = ?1 ORDER BY up_to_position",
    )?;
    let rows = stmt.query_map(params![chat_id], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// Flattens messages into plain text for the summarizer prompt.
fn render_transcript(messages: &[ChatMessage]) -> String {
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
    if out.chars().count() > SUMMARIZER_MAX_CHARS {
        let keep = SUMMARIZER_MAX_CHARS / 2;
        let head: String = out.chars().take(keep).collect();
        let tail: String = out.chars().skip(out.chars().count() - keep).collect();
        out = format!("{head}\n[…part of the conversation skipped…]\n{tail}");
    }
    out
}

fn summarize(backend: &dyn ChatBackend, params: &ChatParams, transcript: &str) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{append_message, ensure_project_chat};
    use crate::db::schema;
    use crate::error::AppError;
    use crate::llm::{ChatEvent, ChatOutcome, ToolCall, ToolSpec};
    use crate::onboarding::temp_test_dir;
    use crate::paths;
    use crate::projects;
    use rusqlite::Connection;
    use serde_json::json;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct FakeBackend {
        reply: std::result::Result<ChatOutcome, String>,
        calls: AtomicUsize,
        prompts: Mutex<Vec<Vec<ChatMessage>>>,
        tool_options: Mutex<Vec<bool>>,
    }

    impl FakeBackend {
        fn ok(reply: &str) -> Self {
            FakeBackend {
                reply: Ok(ChatOutcome {
                    content: Some(reply.into()),
                    reasoning: None,
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: None,
                }),
                calls: AtomicUsize::new(0),
                prompts: Mutex::new(Vec::new()),
                tool_options: Mutex::new(Vec::new()),
            }
        }

        fn failing() -> Self {
            FakeBackend {
                reply: Err("llm down".into()),
                calls: AtomicUsize::new(0),
                prompts: Mutex::new(Vec::new()),
                tool_options: Mutex::new(Vec::new()),
            }
        }
    }

    impl ChatBackend for FakeBackend {
        fn chat_stream(
            &self,
            messages: &[ChatMessage],
            tools: Option<&[ToolSpec]>,
            _params: &ChatParams,
            _on_event: &mut dyn FnMut(ChatEvent),
        ) -> Result<ChatOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.prompts.lock().unwrap().push(messages.to_vec());
            self.tool_options.lock().unwrap().push(tools.is_some());
            self.reply.clone().map_err(AppError::Http)
        }
    }

    fn params() -> ChatParams {
        ChatParams {
            model: "test-model".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
        }
    }

    fn setup(tag: &str) -> (std::path::PathBuf, Connection, i64) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        schema::run_migrations(&conn).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        let dto = projects::create_project(&conn, &wd, "proj").unwrap();
        let chat_id = ensure_project_chat(&conn, dto.id).unwrap();
        (wd, conn, chat_id)
    }

    fn big_text(tag: &str, words: usize) -> String {
        (0..words)
            .map(|i| format!("{tag}{i}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    // Alternating user/assistant turns, each carrying a unique turn marker.
    fn fill(conn: &Connection, chat_id: i64, turns: usize) {
        for i in 0..turns {
            append_message(
                conn,
                chat_id,
                Role::User,
                Some(&format!("turn{i}-u {}", big_text("u", 30))),
                &[],
                None,
                None,
            )
            .unwrap();
            append_message(
                conn,
                chat_id,
                Role::Assistant,
                Some(&format!("turn{i}-a {}", big_text("a", 30))),
                &[],
                None,
                None,
            )
            .unwrap();
        }
    }

    fn summary_rows(conn: &Connection, chat_id: i64) -> Vec<(i64, String)> {
        let mut stmt = conn
            .prepare("SELECT up_to_position, summary FROM summaries WHERE chat_id = ?1 ORDER BY up_to_position")
            .unwrap();
        stmt.query_map(params![chat_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    fn contents_in_range(conn: &Connection, chat_id: i64, from: i64, to: i64) -> Vec<String> {
        let mut stmt = conn
            .prepare(
                "SELECT content FROM messages WHERE chat_id = ?1 AND position > ?2 AND position <= ?3 ORDER BY position",
            )
            .unwrap();
        stmt.query_map(params![chat_id, from, to], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn estimate_tokens_is_monotonic_and_sane() {
        assert_eq!(estimate_tokens(&[]), 0);
        let one = estimate_tokens(&[ChatMessage::user("x".repeat(400))]);
        assert!((100..120).contains(&one), "got {one}");
        let two = estimate_tokens(&[
            ChatMessage::user("x".repeat(400)),
            ChatMessage::user("y".repeat(400)),
        ]);
        assert!(two > one);
        // Tool call payloads count toward the estimate.
        let with_call = estimate_tokens(&[ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            "c1".into(),
            "read_file".into(),
            json!({ "rel_path": big_text("p", 50) }).to_string(),
        )])]);
        assert!(with_call > estimate_tokens(&[ChatMessage::assistant("ok")]));
    }

    #[test]
    fn below_threshold_no_summarization_and_full_history() {
        let (wd, conn, chat_id) = setup("ctx-below");
        fill(&conn, chat_id, 2);
        append_message(&conn, chat_id, Role::User, Some("latest"), &[], None, None).unwrap();
        let backend = FakeBackend::ok("unused");
        let out = build_context(&conn, chat_id, "sys", 100_000, 80, &backend, &params(), false).unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        assert_eq!(out.new_checkpoint_up_to, None);
        assert_eq!(out.messages.len(), 1 + 5);
        assert_eq!(out.messages[0].role, Role::System);
        assert_eq!(out.messages.last().unwrap().text_content(), Some("latest"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn above_threshold_one_summary_persisted_and_tail_kept() {
        let (wd, conn, chat_id) = setup("ctx-above");
        fill(&conn, chat_id, 20);
        append_message(&conn, chat_id, Role::User, Some("latest turn"), &[], None, None).unwrap();
        let backend = FakeBackend::ok("SUMMARY BODY");
        let out = build_context(&conn, chat_id, "sys", 1000, 80, &backend, &params(), false).unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        assert!(!backend.tool_options.lock().unwrap()[0], "summarizer got tools");
        let rows = summary_rows(&conn, chat_id);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "SUMMARY BODY");
        assert_eq!(out.new_checkpoint_up_to, Some(rows[0].0));
        assert_eq!(out.messages[0].role, Role::System);
        assert!(out.messages[1].text_content()
            .unwrap()
            .starts_with(&format!("{SUMMARY_PREFIX}\nSUMMARY BODY")));
        assert_eq!(out.messages.last().unwrap().text_content(), Some("latest turn"));
        assert!(out.messages.len() < 1 + 41);
        // The oldest messages went into the summarization prompt.
        let prompts = backend.prompts.lock().unwrap();
        let transcript = prompts[0][1].text_content().unwrap();
        assert!(transcript.contains("turn0-u"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn second_call_reuses_checkpoint_growth_summarizes_only_new_range() {
        let (wd, conn, chat_id) = setup("ctx-reuse");
        fill(&conn, chat_id, 20);
        append_message(&conn, chat_id, Role::User, Some("latest turn"), &[], None, None).unwrap();
        let backend = FakeBackend::ok("S1");
        let first = build_context(&conn, chat_id, "sys", 1000, 80, &backend, &params(), false).unwrap();
        let up_to = first.new_checkpoint_up_to.unwrap();

        // Same transcript: the stored checkpoint is reused, no new call.
        let second = build_context(&conn, chat_id, "sys", 1000, 80, &backend, &params(), false).unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        assert_eq!(second.new_checkpoint_up_to, None);
        assert_eq!(
            serde_json::to_string(&second.messages).unwrap(),
            serde_json::to_string(&first.messages).unwrap()
        );

        // Growth beyond the checkpoint triggers an incremental summary of
        // only the newly expired range.
        append_message(&conn, chat_id, Role::Assistant, Some("answer"), &[], None, None).unwrap();
        append_message(&conn, chat_id, Role::User, Some("NEW MARKER TURN"), &[], None, None).unwrap();
        let backend2 = FakeBackend::ok("S2");
        let third = build_context(&conn, chat_id, "sys", 1000, 80, &backend2, &params(), false).unwrap();
        assert_eq!(backend2.calls.load(Ordering::SeqCst), 1);
        let new_up_to = third.new_checkpoint_up_to.unwrap();
        assert!(new_up_to > up_to);
        let rows = summary_rows(&conn, chat_id);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1], (new_up_to, "S2".to_string()));

        let prompts = backend2.prompts.lock().unwrap();
        let transcript = prompts[0][1].text_content().unwrap();
        for content in contents_in_range(&conn, chat_id, up_to, new_up_to) {
            assert!(transcript.contains(&content), "new range missing from prompt");
        }
        let oldest = contents_in_range(&conn, chat_id, 0, up_to);
        assert!(!transcript.contains(&oldest[0]), "old range was re-summarized");
        // Both summaries precede the tail; the latest user turn survives.
        let summaries = third
            .messages
            .iter()
            .filter(|m| m.text_content().unwrap_or_default().starts_with(SUMMARY_PREFIX))
            .count();
        assert_eq!(summaries, 2);
        assert_eq!(third.messages.last().unwrap().text_content(), Some("NEW MARKER TURN"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn summarization_failure_falls_back_to_uncompacted_history() {
        let (wd, conn, chat_id) = setup("ctx-fallback");
        fill(&conn, chat_id, 20);
        append_message(&conn, chat_id, Role::User, Some("latest"), &[], None, None).unwrap();
        let backend = FakeBackend::failing();
        let out = build_context(&conn, chat_id, "sys", 1000, 80, &backend, &params(), false).unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        assert_eq!(out.new_checkpoint_up_to, None);
        assert_eq!(out.messages.len(), 1 + 41);
        assert!(summary_rows(&conn, chat_id).is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn boundary_keeps_tail_within_budget_and_never_starts_on_tool_result() {
        let (wd, conn, chat_id) = setup("ctx-boundary");
        // Tool units: user, assistant(calls), tool result; then the user turn.
        for i in 0..15 {
            append_message(&conn, chat_id, Role::User, Some(&big_text("q", 20)), &[], None, None)
                .unwrap();
            let calls = vec![ToolCall::new(
                format!("c{i}"),
                "read_file".into(),
                json!({ "rel_path": big_text("f", 20) }).to_string(),
            )];
            append_message(&conn, chat_id, Role::Assistant, None, &calls, None, None).unwrap();
            append_message(
                &conn,
                chat_id,
                Role::Tool,
                Some(&big_text("r", 40)),
                &[],
                Some(&format!("c{i}")),
                None,
            )
            .unwrap();
        }
        append_message(&conn, chat_id, Role::User, Some("final"), &[], None, None).unwrap();
        let backend = FakeBackend::ok("S");
        let out = build_context(&conn, chat_id, "sys", 1000, 80, &backend, &params(), false).unwrap();
        let tail_start = out
            .messages
            .iter()
            .rposition(|m| {
                m.text_content()
                    .unwrap_or_default()
                    .starts_with(SUMMARY_PREFIX)
            })
            .unwrap()
            + 1;
        let tail = &out.messages[tail_start..];
        assert_ne!(tail[0].role, Role::Tool, "tail starts on an orphaned tool result");
        // Budget is window - threshold = 200 tokens; slack covers the fresh
        // last message (always kept) and one assistant unit pulled back to
        // avoid an orphaned tool result.
        let last_cost = estimate_tokens(&tail[tail.len() - 1..]);
        let slack = if tail[0].role == Role::Assistant {
            estimate_tokens(&tail[..1])
        } else {
            0
        };
        assert!(estimate_tokens(tail) <= 200 + last_cost + slack);
        assert!(tail.len() < 46);
        assert_eq!(tail.last().unwrap().text_content(), Some("final"));
        fs::remove_dir_all(&wd).unwrap();
    }
}
