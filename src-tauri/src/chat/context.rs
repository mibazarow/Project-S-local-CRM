// Context building and compaction for the project agent (SPEC 4.6
// "автосуммаризация при 80% окна", SPEC 6.4 "Сжатие контекста с чекпоинтами").
// When the estimated context crosses the threshold of the model window, the
// oldest messages are folded into checkpoint summaries stored in `summaries`;
// the raw transcript stays append-only and only the model-facing view is
// compacted, so the untouched prefix stays prompt-cache friendly. When
// summarization fails the uncompacted history is still returned, but the
// outcome carries a visible compaction_error so the run can explain why nothing
// was compacted. Before an expired range is folded away, durable facts are
// distilled into global memory through memory::extract_facts (best-effort,
// SPEC 6.3).
//
// Split per R-11: `estimate` — the deterministic Cyrillic-aware token
// estimator; `volatile` — the volatile [Context] block and its attachment to
// the last user turn; `summarize` — the summarizer transcript, its char cap and
// the LLM call; `compact` — the build_context orchestration, boundary picking
// and checkpoint reads. The public surface is re-exported here so call sites
// keep the `chat::context::…` paths.
mod compact;
mod estimate;
mod summarize;
mod volatile;

pub use compact::build_context;
pub use volatile::volatile_block;
pub(crate) use summarize::render_transcript_capped;
// Reached only from tests: the context tests below use the token estimator and
// the summary header, the memory FactExtractor tests use the summarizer cap.
// The rest (ContextOutcome, KEEP_TAIL_PERCENT, CONTEXT_BLOCK_PREFIX) stay `pub`
// inside the module family — no caller outside references them, so they are not
// re-exported (a `pub use` of an unreferenced name trips unused_imports).
#[cfg(test)]
pub(crate) use compact::SUMMARY_PREFIX;
#[cfg(test)]
pub(crate) use estimate::{estimate_text_tokens, estimate_tokens};
#[cfg(test)]
pub(crate) use summarize::SUMMARIZER_MAX_CHARS;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::r#loop::ChatBackend;
    use crate::chat::{append_message, ensure_project_chat};
    use crate::db::schema;
    use crate::error::{AppError, Result};
    use crate::llm::{ChatEvent, ChatMessage, ChatOutcome, ChatParams, Role, ToolCall, ToolSpec};
    use crate::onboarding::temp_test_dir;
    use crate::paths;
    use crate::projects;
    use rusqlite::{params, Connection};
    use serde_json::json;
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct FakeBackend {
        reply: std::result::Result<ChatOutcome, String>,
        // When non-empty, replies are taken from the queue first (one per
        // call: fact extraction, then summarization), then `reply` repeats.
        queue: Mutex<VecDeque<std::result::Result<ChatOutcome, String>>>,
        calls: AtomicUsize,
        prompts: Mutex<Vec<Vec<ChatMessage>>>,
        tool_options: Mutex<Vec<bool>>,
    }

    impl FakeBackend {
        fn outcome(reply: &str) -> ChatOutcome {
            ChatOutcome {
                content: Some(reply.into()),
                reasoning: None,
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
            }
        }

        fn ok(reply: &str) -> Self {
            FakeBackend {
                reply: Ok(Self::outcome(reply)),
                queue: Mutex::new(VecDeque::new()),
                calls: AtomicUsize::new(0),
                prompts: Mutex::new(Vec::new()),
                tool_options: Mutex::new(Vec::new()),
            }
        }

        fn failing() -> Self {
            FakeBackend {
                reply: Err("llm down".into()),
                queue: Mutex::new(VecDeque::new()),
                calls: AtomicUsize::new(0),
                prompts: Mutex::new(Vec::new()),
                tool_options: Mutex::new(Vec::new()),
            }
        }

        fn queued(replies: Vec<std::result::Result<ChatOutcome, String>>) -> Self {
            FakeBackend {
                reply: Err("queue exhausted".into()),
                queue: Mutex::new(replies.into()),
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
            let reply = {
                let mut queue = self.queue.lock().unwrap();
                match queue.pop_front() {
                    Some(reply) => reply,
                    None => self.reply.clone(),
                }
            };
            reply.map_err(AppError::Http)
        }
    }

    fn params() -> ChatParams {
        ChatParams {
            model: "test-model".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            requires_reasoning_passback: false,
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

    fn setup_file(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, i64) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        crate::db::ensure_vec_extension();
        let db_path = paths::db_path(&wd);
        let conn = crate::db::open(&db_path).unwrap();
        let dto = projects::create_project(&conn, &wd, "proj").unwrap();
        let chat_id = ensure_project_chat(&conn, dto.id).unwrap();
        drop(conn);
        (wd, db_path, chat_id)
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
        // Thinking traces are sent back to the provider on assistant turns, so
        // they count toward the window estimate.
        let plain = ChatMessage::assistant("ok");
        let thinking = plain.clone().with_reasoning(Some("x".repeat(400)));
        assert!(estimate_tokens(&[thinking]) >= estimate_tokens(&[plain]) + 100);
    }

    #[test]
    fn cyrillic_text_estimates_at_about_two_chars_per_token() {
        // 200 Cyrillic chars: ~100 tokens, not the ~50 a plain chars/4 rule
        // would report for the same length.
        let russian = estimate_text_tokens(&"ы".repeat(200));
        assert!((95..105).contains(&russian), "got {russian}");
        let english = estimate_text_tokens(&"x".repeat(200));
        assert!((48..53).contains(&english), "got {english}");
        assert!(russian > english, "Cyrillic must not be underestimated");

        // Mixed text: only the Cyrillic letters get the denser rate.
        // "Привет мир": 9 Cyrillic (9/2=4) + 1 space (1/4=0).
        assert_eq!(estimate_text_tokens("Привет мир"), 4);
        // "hello world": 11 ASCII -> 11/4=2. Ё counts as Cyrillic: 4/4=2.
        assert_eq!(estimate_text_tokens("hello world"), 2);
        assert_eq!(estimate_text_tokens("Ёлка"), 2);

        // Message level: the same char count costs more in Russian.
        let ru = estimate_tokens(&[ChatMessage::user("привет мир ".repeat(20))]);
        let en = estimate_tokens(&[ChatMessage::user("hello world ".repeat(20))]);
        assert!(ru > en, "ru={ru} en={en}");
    }

    #[test]
    fn below_threshold_no_summarization_and_full_history() {
        let (wd, conn, chat_id) = setup("ctx-below");
        fill(&conn, chat_id, 2);
        append_message(&conn, chat_id, Role::User, Some("latest"), &[], None, None).unwrap();
        let backend = FakeBackend::ok("unused");
        let out = build_context(&conn, chat_id, "sys", "", 100_000, 80, &backend, &params(), false).unwrap();
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
        let out = build_context(&conn, chat_id, "sys", "", 1000, 80, &backend, &params(), false).unwrap();
        // Two LLM calls per fold: fact extraction first, then summarization.
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert!(
            backend.tool_options.lock().unwrap().iter().all(|t| !t),
            "fact extractor/summarizer got tools"
        );
        let rows = summary_rows(&conn, chat_id);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "SUMMARY BODY");
        assert_eq!(out.new_checkpoint_up_to, Some(rows[0].0));
        // The summary text rides the outcome for the UI event and is never
        // persisted as a chat message (only the `summaries` table holds it).
        assert_eq!(out.new_summary.as_deref(), Some("SUMMARY BODY"));
        let as_message: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages WHERE chat_id = ?1 AND content LIKE '%SUMMARY BODY%'",
                params![chat_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(as_message, 0);
        assert_eq!(out.messages[0].role, Role::System);
        assert!(out.messages[1].text_content()
            .unwrap()
            .starts_with(&format!("{SUMMARY_PREFIX}\nSUMMARY BODY")));
        assert_eq!(out.messages.last().unwrap().text_content(), Some("latest turn"));
        assert!(out.messages.len() < 1 + 41);
        // The oldest messages went into the summarization prompt (the second
        // call; the first one is the fact extractor over the same range).
        let prompts = backend.prompts.lock().unwrap();
        let transcript = prompts[1][1].text_content().unwrap();
        assert!(transcript.contains("turn0-u"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn second_call_reuses_checkpoint_growth_summarizes_only_new_range() {
        let (wd, conn, chat_id) = setup("ctx-reuse");
        fill(&conn, chat_id, 20);
        append_message(&conn, chat_id, Role::User, Some("latest turn"), &[], None, None).unwrap();
        let backend = FakeBackend::ok("S1");
        let first = build_context(&conn, chat_id, "sys", "", 1000, 80, &backend, &params(), false).unwrap();
        let up_to = first.new_checkpoint_up_to.unwrap();

        // Same transcript: the stored checkpoint is reused, no new calls.
        let second = build_context(&conn, chat_id, "sys", "", 1000, 80, &backend, &params(), false).unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert_eq!(second.new_checkpoint_up_to, None);
        assert_eq!(
            serde_json::to_string(&second.messages).unwrap(),
            serde_json::to_string(&first.messages).unwrap()
        );

        // Growth beyond the checkpoint triggers an incremental summary of
        // only the newly expired range. The kept tail is large enough that two
        // short turns do NOT move the boundary — that is the hysteresis — so
        // the new turn carries real content.
        append_message(&conn, chat_id, Role::Assistant, Some("answer"), &[], None, None).unwrap();
        append_message(
            &conn,
            chat_id,
            Role::User,
            Some(&format!("NEW MARKER TURN {}", big_text("n", 30))),
            &[],
            None,
            None,
        )
        .unwrap();
        let backend2 = FakeBackend::ok("S2");
        let third = build_context(&conn, chat_id, "sys", "", 1000, 80, &backend2, &params(), false).unwrap();
        assert_eq!(backend2.calls.load(Ordering::SeqCst), 2);
        let new_up_to = third.new_checkpoint_up_to.unwrap();
        assert!(new_up_to > up_to);
        let rows = summary_rows(&conn, chat_id);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1], (new_up_to, "S2".to_string()));

        let prompts = backend2.prompts.lock().unwrap();
        let transcript = prompts[1][1].text_content().unwrap();
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
        assert!(third
            .messages
            .last()
            .unwrap()
            .text_content()
            .unwrap()
            .starts_with("NEW MARKER TURN"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn summarization_failure_returns_uncompacted_history_with_a_visible_error() {
        let (wd, conn, chat_id) = setup("ctx-fallback");
        fill(&conn, chat_id, 20);
        append_message(&conn, chat_id, Role::User, Some("latest"), &[], None, None).unwrap();
        let backend = FakeBackend::failing();
        let out = build_context(&conn, chat_id, "sys", "", 1000, 80, &backend, &params(), false).unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert_eq!(out.new_checkpoint_up_to, None);
        assert_eq!(out.messages.len(), 1 + 41);
        let error = out.compaction_error.expect("compaction failure must be reported");
        assert!(error.contains("Не удалось сжать контекст"), "{error}");
        assert!(error.contains("без сжатия"), "{error}");
        assert!(summary_rows(&conn, chat_id).is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn single_giant_message_that_cannot_be_folded_reports_compaction_failure() {
        let (wd, conn, chat_id) = setup("ctx-giant-last");
        append_message(
            &conn,
            chat_id,
            Role::User,
            Some(&big_text("giant", 5000)),
            &[],
            None,
            None,
        )
        .unwrap();
        let backend = FakeBackend::ok("unused");
        let out = build_context(&conn, chat_id, "sys", "", 1000, 80, &backend, &params(), false).unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        assert_eq!(out.new_checkpoint_up_to, None);
        assert!(out.compaction_error.unwrap().contains("не помещается в окно модели"));
        assert!(out.messages.last().unwrap().text_content().unwrap().starts_with("giant"));
        fs::remove_dir_all(&wd).unwrap();
    }

    // Durable facts (SPEC 6.3): the expired range is distilled into global
    // memory (source = "fact") before it disappears into the summary; the
    // fact call comes first, duplicates collapse, the fold still happens.
    #[test]
    fn durable_facts_are_extracted_before_summarizing_the_expired_range() {
        let (wd, conn, chat_id) = setup("ctx-facts");
        fill(&conn, chat_id, 20);
        append_message(&conn, chat_id, Role::User, Some("latest"), &[], None, None).unwrap();
        let backend = FakeBackend::ok(
            "[\"Пользователь любит кофе\", \"пользователь любит кофе\", \"Клиент example.ru — аптечная сеть\"]",
        );
        let out = build_context(&conn, chat_id, "sys", "", 1000, 80, &backend, &params(), false).unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        // The fact extractor ran first, over the same expired range.
        let prompts = backend.prompts.lock().unwrap();
        assert!(prompts[0][0].text_content().unwrap().contains("extract durable facts"));
        assert!(prompts[0][1].text_content().unwrap().contains("turn0-u"));
        drop(prompts);

        let facts: Vec<String> = conn
            .prepare("SELECT content FROM memory WHERE source = 'fact' ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(facts, vec!["Пользователь любит кофе", "Клиент example.ru — аптечная сеть"]);
        // Facts of a project chat are attached to that project.
        let project_id: i64 = conn
            .query_row("SELECT project_id FROM chats WHERE id = ?1", params![chat_id], |r| r.get(0))
            .unwrap();
        let attached: i64 = conn
            .query_row("SELECT count(*) FROM memory WHERE source = 'fact' AND project_id = ?1", params![project_id], |r| r.get(0))
            .unwrap();
        assert_eq!(attached, 2);

        // Compaction itself is unaffected: the checkpoint row exists.
        let rows = summary_rows(&conn, chat_id);
        assert_eq!(rows.len(), 1);
        assert_eq!(out.new_checkpoint_up_to, Some(rows[0].0));
        fs::remove_dir_all(&wd).unwrap();
    }

    // A failing fact extraction must not break compaction: the fold and the
    // checkpoint row survive on the summarizer's success alone.
    #[test]
    fn fact_extraction_failure_does_not_break_compaction() {
        let (wd, conn, chat_id) = setup("ctx-facts-fail");
        fill(&conn, chat_id, 20);
        append_message(&conn, chat_id, Role::User, Some("latest"), &[], None, None).unwrap();
        let backend = FakeBackend::queued(vec![
            Err("fact extractor down".into()),
            Ok(FakeBackend::outcome("SUMMARY BODY")),
        ]);
        let out = build_context(&conn, chat_id, "sys", "", 1000, 80, &backend, &params(), false).unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        let rows = summary_rows(&conn, chat_id);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "SUMMARY BODY");
        assert_eq!(out.new_checkpoint_up_to, Some(rows[0].0));
        let facts: i64 = conn
            .query_row("SELECT count(*) FROM memory WHERE source = 'fact'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(facts, 0);
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
        let out = build_context(&conn, chat_id, "sys", "", 1000, 80, &backend, &params(), false).unwrap();
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
        // Budget is KEEP_TAIL_PERCENT = 450 tokens; slack covers the fresh
        // last message (always kept) and one assistant unit pulled back to
        // avoid an orphaned tool result.
        let last_cost = estimate_tokens(&tail[tail.len() - 1..]);
        let slack = if tail[0].role == Role::Assistant {
            estimate_tokens(&tail[..1])
        } else {
            0
        };
        assert!(estimate_tokens(tail) <= 450 + last_cost + slack);
        assert!(tail.len() < 46);
        assert_eq!(tail.last().unwrap().text_content(), Some("final"));
        fs::remove_dir_all(&wd).unwrap();
    }

    // Compaction must never leave an open tool_calls group in the model-facing
    // output: the fold boundary, the summary turns and the kept tail all have
    // to keep every assistant tool_calls turn answered by tool messages.
    #[test]
    fn compaction_output_keeps_every_tool_call_group_closed() {
        let (wd, conn, chat_id) = setup("ctx-sanitize");
        // Tool units with two calls each: assistant(c{i}a, c{i}b) + two results.
        for i in 0..15 {
            append_message(&conn, chat_id, Role::User, Some(&big_text("q", 20)), &[], None, None)
                .unwrap();
            let calls = vec![
                ToolCall::new(format!("c{i}a"), "read_file".into(), json!({ "rel_path": big_text("f", 20) }).to_string()),
                ToolCall::new(format!("c{i}b"), "read_file".into(), json!({ "rel_path": big_text("g", 20) }).to_string()),
            ];
            append_message(&conn, chat_id, Role::Assistant, None, &calls, None, None).unwrap();
            append_message(&conn, chat_id, Role::Tool, Some(&big_text("r", 40)), &[], Some(&format!("c{i}a")), None)
                .unwrap();
            append_message(&conn, chat_id, Role::Tool, Some(&big_text("s", 40)), &[], Some(&format!("c{i}b")), None)
                .unwrap();
        }
        append_message(&conn, chat_id, Role::User, Some("final"), &[], None, None).unwrap();
        let backend = FakeBackend::ok("S");
        let out = build_context(&conn, chat_id, "sys", "", 1000, 80, &backend, &params(), false).unwrap();
        assert!(
            tool_groups_are_closed(&out.messages),
            "compacted context has an open tool_calls group"
        );
        assert_eq!(out.messages.last().unwrap().text_content(), Some("final"));
        fs::remove_dir_all(&wd).unwrap();
    }

    fn tool_groups_are_closed(messages: &[ChatMessage]) -> bool {
        let mut pending: Vec<String> = Vec::new();
        for m in messages {
            if m.role == Role::Tool {
                match m.tool_call_id.as_ref().and_then(|id| pending.iter().position(|p| p == id)) {
                    Some(pos) => { pending.remove(pos); }
                    None => return false,
                }
                continue;
            }
            if !pending.is_empty() {
                return false;
            }
            if m.role == Role::Assistant {
                pending = m.tool_calls.iter().map(|tc| tc.id.clone()).collect();
            }
        }
        pending.is_empty()
    }

    // Hysteresis: the kept tail is sized to at most 45% of the window, not
    // the leftover window - threshold (~20% at the 80% trigger), so one fold
    // leaves real headroom before the next compaction can fire.
    #[test]
    fn compaction_tail_is_sized_to_forty_five_percent_of_the_window() {
        let (wd, conn, chat_id) = setup("ctx-hysteresis");
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
        let out = build_context(&conn, chat_id, "sys", "", 1000, 80, &backend, &params(), false).unwrap();
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
        // The latest user turn survives the fold.
        assert_eq!(tail.last().unwrap().text_content(), Some("final"));
        // Tail fits the 45% budget (plus the always-kept last message and a
        // pulled-back assistant unit), and it is far larger than the old 20%
        // (window - threshold = 200) budget would have allowed.
        let kept = &tail[..tail.len() - 1];
        assert!(
            estimate_tokens(kept) > 200,
            "kept tail is only {} tokens — hysteresis budget not applied",
            estimate_tokens(kept)
        );
        let last_cost = estimate_tokens(&tail[tail.len() - 1..]);
        let slack = if tail[0].role == Role::Assistant {
            estimate_tokens(&tail[..1])
        } else {
            0
        };
        assert!(estimate_tokens(tail) <= 450 + last_cost + slack);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn small_window_compacts_a_long_history_and_keeps_the_chat_going() {
        let (wd, conn, chat_id) = setup("ctx-small-long");
        fill(&conn, chat_id, 120);
        append_message(&conn, chat_id, Role::User, Some("latest turn"), &[], None, None).unwrap();
        let backend = FakeBackend::ok("SUMMARY BODY");
        let out = build_context(&conn, chat_id, "sys", "", 8000, 80, &backend, &params(), false).unwrap();
        assert!(out.compaction_error.is_none(), "{:?}", out.compaction_error);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert_eq!(summary_rows(&conn, chat_id).len(), 1);
        assert_eq!(out.messages.last().unwrap().text_content(), Some("latest turn"));
        assert!(out.messages.len() < 1 + 241);
        assert!(
            estimate_tokens(&out.messages) <= 6400,
            "post-fold context is {} tokens",
            estimate_tokens(&out.messages)
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn small_window_folds_a_giant_older_message_into_the_summary() {
        let (wd, conn, chat_id) = setup("ctx-small-giant");
        fill(&conn, chat_id, 30);
        append_message(
            &conn,
            chat_id,
            Role::User,
            Some(&format!("GIANT {}", big_text("g", 5000))),
            &[],
            None,
            None,
        )
        .unwrap();
        append_message(&conn, chat_id, Role::Assistant, Some("ok"), &[], None, None).unwrap();
        fill(&conn, chat_id, 60);
        append_message(&conn, chat_id, Role::User, Some("latest"), &[], None, None).unwrap();
        let backend = FakeBackend::ok("SUMMARY BODY");
        let out = build_context(&conn, chat_id, "sys", "", 8000, 80, &backend, &params(), false).unwrap();
        assert!(out.compaction_error.is_none(), "{:?}", out.compaction_error);
        let up_to = out.new_checkpoint_up_to.expect("giant history must be folded");
        // The giant message (position 61) is inside the summarized range.
        assert!(up_to > 61);
        let prompts = backend.prompts.lock().unwrap();
        let transcript = prompts[1][1].text_content().unwrap();
        assert!(
            transcript.chars().count() <= 8000,
            "summarizer request does not fit the 8k window"
        );
        drop(prompts);
        assert_eq!(out.messages.last().unwrap().text_content(), Some("latest"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn small_window_compacts_many_tool_results_without_open_groups() {
        let (wd, conn, chat_id) = setup("ctx-small-tools");
        for i in 0..60 {
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
                Some(&big_text("r", 120)),
                &[],
                Some(&format!("c{i}")),
                None,
            )
            .unwrap();
        }
        append_message(&conn, chat_id, Role::User, Some("final"), &[], None, None).unwrap();
        let backend = FakeBackend::ok("SUMMARY BODY");
        let out = build_context(&conn, chat_id, "sys", "", 8000, 80, &backend, &params(), false).unwrap();
        assert!(out.compaction_error.is_none(), "{:?}", out.compaction_error);
        assert!(out.new_checkpoint_up_to.is_some());
        assert!(tool_groups_are_closed(&out.messages));
        assert_eq!(out.messages.last().unwrap().text_content(), Some("final"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn restart_reuses_stored_summaries_without_resummarizing() {
        let (wd, db_path, chat_id) = setup_file("ctx-restart");
        {
            let conn = crate::db::open(&db_path).unwrap();
            fill(&conn, chat_id, 120);
            append_message(&conn, chat_id, Role::User, Some("latest"), &[], None, None).unwrap();
            let backend = FakeBackend::ok("PERSISTED SUMMARY");
            let out = build_context(&conn, chat_id, "sys", "", 8000, 80, &backend, &params(), false).unwrap();
            assert_eq!(out.new_summary.as_deref(), Some("PERSISTED SUMMARY"));
        }
        let conn = crate::db::open(&db_path).unwrap();
        let restart_backend = FakeBackend::failing();
        let out = build_context(
            &conn,
            chat_id,
            "sys",
            "",
            8000,
            80,
            &restart_backend,
            &params(),
            false,
        )
        .unwrap();
        assert_eq!(restart_backend.calls.load(Ordering::SeqCst), 0);
        assert!(out.compaction_error.is_none());
        assert!(out
            .messages
            .iter()
            .any(|m| m.text_content().unwrap_or_default().contains("PERSISTED SUMMARY")));
        assert_eq!(summary_rows(&conn, chat_id).len(), 1);
        drop(conn);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn post_fold_context_stays_below_the_trigger_with_system_and_summary() {
        let (wd, conn, chat_id) = setup("ctx-post-fold-budget");
        fill(&conn, chat_id, 120);
        append_message(&conn, chat_id, Role::User, Some("latest"), &[], None, None).unwrap();
        let system = "s".repeat(4000);
        let summary = "x".repeat(6400);
        let backend = FakeBackend::ok(&summary);
        let out = build_context(&conn, chat_id, &system, "", 8000, 80, &backend, &params(), false).unwrap();
        assert!(out.new_checkpoint_up_to.is_some());
        assert!(
            estimate_tokens(&out.messages) <= 6400,
            "post-fold context is {} tokens, threshold is 6400",
            estimate_tokens(&out.messages)
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn volatile_block_lands_on_the_last_user_turn_before_the_user_text() {
        let (wd, conn, chat_id) = setup("ctx-volatile");
        append_message(&conn, chat_id, Role::User, Some("older question"), &[], None, None).unwrap();
        append_message(&conn, chat_id, Role::Assistant, Some("older answer"), &[], None, None).unwrap();
        append_message(&conn, chat_id, Role::User, Some("MARKER QUESTION"), &[], None, None).unwrap();
        let backend = FakeBackend::ok("unused");
        let block = volatile_block(&[
            "Project brief (updated 2026-10-17 12:00:00):\nBRIEF BODY",
            "Relevant memory records:\n- MEMORY MARKER",
        ]);
        let out =
            build_context(&conn, chat_id, "sys", &block, 100_000, 80, &backend, &params(), false)
                .unwrap();
        let last = out.messages.last().unwrap().text_content().unwrap();
        assert!(last.starts_with("[Context]"), "{last}");
        assert!(last.contains("BRIEF BODY") && last.contains("MEMORY MARKER"), "{last}");
        let brief_at = last.find("BRIEF BODY").unwrap();
        let memory_at = last.find("MEMORY MARKER").unwrap();
        let user_at = last.find("MARKER QUESTION").unwrap();
        assert!(brief_at < user_at && memory_at < user_at, "context must precede the user text");
        // Older turns never receive the block…
        assert_eq!(out.messages[1].text_content(), Some("older question"));
        // …and the stored row stays plain: the block is model-facing only.
        let raw: String = conn
            .query_row(
                "SELECT content FROM messages WHERE chat_id = ?1 ORDER BY position DESC LIMIT 1",
                params![chat_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw, "MARKER QUESTION");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn system_prompt_is_identical_for_different_briefs_and_memory_queries() {
        let (wd, conn, chat_id) = setup("ctx-stable-prefix");
        append_message(&conn, chat_id, Role::User, Some("вопрос"), &[], None, None).unwrap();
        let backend = FakeBackend::ok("unused");
        let block_a = volatile_block(&[
            "Project brief (updated 2026-10-01 10:00:00):\nBRIEF A",
            "Relevant memory records:\n- REC A",
        ]);
        let block_b = volatile_block(&["Project brief (updated 2026-10-17 12:00:00):\nBRIEF B"]);
        let a =
            build_context(&conn, chat_id, "sys", &block_a, 100_000, 80, &backend, &params(), false)
                .unwrap();
        let b =
            build_context(&conn, chat_id, "sys", &block_b, 100_000, 80, &backend, &params(), false)
                .unwrap();
        // The cacheable prefix (system turn) is byte-identical for both runs…
        assert_eq!(
            serde_json::to_string(&a.messages[0]).unwrap(),
            serde_json::to_string(&b.messages[0]).unwrap()
        );
        // …only the volatile tail turn differs.
        assert_ne!(
            a.messages.last().unwrap().text_content().unwrap(),
            b.messages.last().unwrap().text_content().unwrap()
        );
        // An empty volatile block prepends nothing.
        let plain =
            build_context(&conn, chat_id, "sys", "", 100_000, 80, &backend, &params(), false)
                .unwrap();
        assert_eq!(plain.messages.last().unwrap().text_content(), Some("вопрос"));
        fs::remove_dir_all(&wd).unwrap();
    }
}
