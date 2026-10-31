// Compaction orchestration (SPEC 4.6 "автосуммаризация при 80% окна",
// SPEC 6.4 "Сжатие контекста с чекпоинтами"). build_context assembles the
// model-facing transcript (system prompt + stored checkpoint summaries + the
// recent tail); when the estimate crosses the threshold of the model window it
// folds the oldest messages into a new checkpoint — distilling durable facts
// into global memory first (best-effort, SPEC 6.3), then summarizing only the
// range beyond the last checkpoint. The raw transcript stays append-only and
// only the model-facing view is compacted, so the untouched prefix stays
// prompt-cache friendly. The kept tail is sized by hysteresis
// (KEEP_TAIL_PERCENT) and never starts on an orphaned tool result. When
// summarization fails the uncompacted history is still returned, but the
// outcome carries a visible compaction_error so the run can explain why
// nothing was compacted.
use super::estimate::estimate_tokens;
use super::summarize::{render_transcript_capped, summarize, summarizer_char_cap};
use super::volatile::attach_volatile_block;
use crate::agent::r#loop::ChatBackend;
use crate::chat::load_history;
use crate::error::Result;
use crate::llm::{ChatMessage, ChatParams, Role};
use rusqlite::{params, Connection};

// Compaction trigger defaults to 80% of the model context window (SPEC 4.6);
// the value lives in app.json `compactionThresholdPercent` (AppConfig).
// Header of the synthetic message that replaces a summarized range.
pub const SUMMARY_PREFIX: &str = "[Summary of the earlier conversation]";

// Hysteresis: after a compaction the kept tail is sized to this share of the
// window, not to the leftover `window - threshold` (~20% at the 80% trigger).
// 45% leaves enough headroom for the system prompt, stored checkpoints and a
// fresh summary; at 60% those three alone could push the context back over the
// 80% trigger (and, on small windows, over the provider limit) right after a
// fold.
pub const KEEP_TAIL_PERCENT: usize = 45;

// Part of the window reserved for the summary being written now: existing
// summaries are measured, the new one is not known until the summarizer
// returns, so the tail budget leaves this share free.
const SUMMARY_RESERVE_PERCENT: usize = 15;

pub struct ContextOutcome {
    pub messages: Vec<ChatMessage>,
    // Set when this call wrote a new checkpoint row (for the UI event).
    pub new_checkpoint_up_to: Option<i64>,
    // The summary text of that new checkpoint (for the UI event): it rides
    // the event only, never persisted as a chat message.
    pub new_summary: Option<String>,
    // Set when the context crossed the threshold but could not be compacted.
    // The run keeps the uncompacted transcript and surfaces this message in
    // the chat instead of failing silently.
    pub compaction_error: Option<String>,
}

// Builds the model-facing transcript: system prompt + checkpoint summaries +
// the recent tail, with the volatile `[Context]` block prepended to the last
// user turn, compacting the oldest messages when the estimate crosses
// `threshold_percent` of `context_window`. `chat_id` must already contain the
// fresh user turn: the last message is never dropped.
#[allow(clippy::too_many_arguments)]
pub fn build_context(
    conn: &Connection,
    chat_id: i64,
    system_prompt: &str,
    volatile_block: &str,
    context_window: u64,
    threshold_percent: u8,
    backend: &dyn ChatBackend,
    params: &ChatParams,
    vision_supported: bool,
) -> Result<ContextOutcome> {
    let mut history = load_history(conn, chat_id, vision_supported)?;
    attach_volatile_block(&mut history, volatile_block);
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
            new_summary: None,
            compaction_error: None,
        });
    }

    let summaries = load_summaries(conn, chat_id)?;
    // messages is append-only with contiguous positions 1..=N, so a message's
    // index in `history` equals its position minus one.
    let last_up_to: i64 = conn.query_row(
        "SELECT COALESCE(MAX(up_to_position), 0) FROM summaries WHERE chat_id = ?1",
        params![chat_id],
        |r| r.get(0),
    )?;
    // The trigger must be measured on the view the model actually receives.
    // The raw transcript keeps growing after a fold (the summary replaces it
    // only for the request), so estimating `full` would keep the chat in
    // "compacting" mode forever and fold the newest 1–3 messages on every
    // turn. Re-check the already-compacted view first and only fold again
    // when it genuinely crosses the threshold.
    if !summaries.is_empty() {
        let kept = (last_up_to.max(0) as usize).min(history.len());
        let mut view = Vec::with_capacity(summaries.len() + 1 + history.len() - kept);
        view.push(system.clone());
        for summary in &summaries {
            view.push(summary_message(summary));
        }
        view.extend(history[kept..].iter().cloned());
        if estimate_tokens(&view) <= threshold_tokens {
            return Ok(ContextOutcome {
                messages: view,
                new_checkpoint_up_to: None,
                new_summary: None,
                compaction_error: None,
            });
        }
    }
    // Existing checkpoints and the system prompt are already fixed costs of
    // the compacted view; the tail budget must leave room for them plus the
    // summary about to be written.
    let fixed_tokens = estimate_tokens(std::slice::from_ref(&system))
        + summaries
            .iter()
            .map(|summary| {
                estimate_tokens(&[ChatMessage::assistant(format!("{SUMMARY_PREFIX}\n{summary}"))])
            })
            .sum::<usize>();
    let reserve_tokens =
        (context_window as usize).saturating_mul(SUMMARY_RESERVE_PERCENT) / 100;
    let tail_cap = (context_window as usize).saturating_mul(KEEP_TAIL_PERCENT) / 100;
    let tail_budget = tail_cap.min(
        threshold_tokens
            .saturating_sub(fixed_tokens)
            .saturating_sub(reserve_tokens),
    );
    let budget_boundary = pick_boundary(&history, tail_budget);
    let boundary = budget_boundary.max(last_up_to as usize);
    if boundary == 0 || boundary >= history.len() {
        return Ok(ContextOutcome {
            messages: full,
            new_checkpoint_up_to: None,
            new_summary: None,
            compaction_error: Some(
                "Не удалось сжать контекст: даже последнее сообщение не помещается в окно модели. \
Сократи сообщение или начни новый чат."
                    .into(),
            ),
        });
    }

    let transcript_cap = summarizer_char_cap(context_window);
    let mut messages = vec![system];
    for summary in &summaries {
        messages.push(summary_message(summary));
    }
    let mut new_checkpoint_up_to = None;
    let mut new_summary = None;
    if boundary as i64 > last_up_to {
        let expired = &history[last_up_to as usize..boundary];
        // Durable facts (SPEC 6.3): before the expired range disappears into
        // a summary, distill stable user/project facts into global memory
        // (source = "fact"). Best-effort: any failure must not break
        // compaction, and facts already stored verbatim are not re-added.
        if let Ok(facts) = crate::memory::extract_facts_capped(backend, params, expired, transcript_cap) {
            let chat_project: Option<i64> = conn
                .query_row("SELECT project_id FROM chats WHERE id = ?1", params![chat_id], |r| r.get(0))
                .unwrap_or(None);
            for fact in facts {
                let known: bool = conn
                    .query_row("SELECT EXISTS(SELECT 1 FROM memory WHERE content = ?1)", params![&fact], |r| r.get(0))
                    .unwrap_or(false);
                if !known {
                    let _ = crate::memory::add(conn, &fact, chat_project, None, Some(crate::memory::FACT_SOURCE));
                }
            }
        }
        // Only the range beyond the last checkpoint is summarized; stored
        // checkpoints are reused, never re-summarized.
        let transcript = render_transcript_capped(expired, transcript_cap);
        match summarize(backend, params, &transcript) {
            Ok(summary) => {
                conn.execute(
                    "INSERT INTO summaries (chat_id, up_to_position, summary, created_at)
                     VALUES (?1, ?2, ?3, datetime('now','localtime'))",
                    params![chat_id, boundary as i64, &summary],
                )?;
                messages.push(summary_message(&summary));
                new_checkpoint_up_to = Some(boundary as i64);
                new_summary = Some(summary);
            }
            // LLM failure: keep the uncompacted history but say so explicitly;
            // the run emits a visible compaction-failed status item.
            Err(e) => {
                return Ok(ContextOutcome {
                    messages: full,
                    new_checkpoint_up_to: None,
                    new_summary: None,
                    compaction_error: Some(format!(
                        "Не удалось сжать контекст: {e}. История отправлена без сжатия — ответ \
может не поместиться в лимит модели; начни новый чат или уменьши окно контекста."
                    )),
                })
            }
        }
    }
    messages.extend(history[boundary..].iter().cloned());
    Ok(ContextOutcome {
        messages,
        new_checkpoint_up_to,
        new_summary,
        compaction_error: None,
    })
}

fn summary_message(summary: &str) -> ChatMessage {
    ChatMessage::assistant(format!("{SUMMARY_PREFIX}\n{summary}"))
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
