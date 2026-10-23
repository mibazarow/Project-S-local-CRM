// Final-answer persistence for a finished run (SPEC 6): the finish_task answer
// lives in the tool call arguments, not in the transcript, so the final
// assistant turn is appended after the loop ends. The per-message transcript
// mirror (on_message) and the run thread itself live in `bridge`.
use crate::agent::r#loop as agent_loop;
use crate::chat::append_full;
use crate::error::Result;
use crate::llm::Role;
use rusqlite::Connection;

// The finish_task answer lives in the tool call arguments, not in the
// transcript, so the final assistant turn is appended after the run. A
// step_limit run already persisted its fallback text through the hook; its
// answer is the explicit stop notice naming the limit, so it is persisted too.
pub(crate) fn persist_final_answer(
    conn: &Connection,
    chat_id: i64,
    result: &agent_loop::AgentRunResult,
) -> Result<()> {
    if !result.answer.trim().is_empty() {
        append_full(
            conn,
            chat_id,
            Role::Assistant,
            Some(&result.answer),
            &[],
            None,
            None,
            result.reasoning.as_deref(),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{catalog_with, finish_call, test_config, tools_outcome, ScriptedBackend};
    use super::*;
    use crate::agent::r#loop::ChatBackend;
    use crate::agent::tool::{EchoTool, PermissionPolicy};
    use crate::chat::test_support::{call, setup};
    use crate::chat::{append_chat_message, append_message, chat_history, ensure_project_chat, load_history};
    use crate::confirm::{AutoConfirmer, DenyAllConfirmer};
    use crate::error::AppError;
    use crate::llm::{ChatEvent, ChatMessage, ChatOutcome, ChatParams, ToolSpec};
    use serde_json::json;
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    fn text_outcome(content: &str) -> ChatOutcome {
        ChatOutcome {
            content: Some(content.into()),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
        }
    }

    #[test]
    fn scripted_run_persists_transcript_and_emits_events_in_order() {
        let (wd, conn, pid) = setup("chat-scripted-run");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        append_message(&conn, chat_id, Role::User, Some("go"), &[], None, None).unwrap();

        let mut thinking_round = tools_outcome(vec![call("c1", "echo", json!({ "text": "hi" }))]);
        thinking_round.reasoning = Some("planning the echo".into());
        let backend = ScriptedBackend::new(vec![
            thinking_round,
            tools_outcome(vec![finish_call("final answer")]),
        ]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Skip)));
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let mut on_message = |msg: &ChatMessage| {
            append_chat_message(&conn, chat_id, msg).unwrap();
        };
        let result = agent_loop::run_with(
            &mut state,
            &test_config("go"),
            &DenyAllConfirmer,
            |e| events.push(e),
            &mut on_message,
        )
        .unwrap();
        persist_final_answer(&conn, chat_id, &result).unwrap();

        // Events in loop order: tool started/finished, finish_task, Done.
        let kinds: Vec<String> = events
            .iter()
            .map(|e| match e {
                agent_loop::AgentEvent::ToolCallStarted { .. } => "started".into(),
                agent_loop::AgentEvent::ToolCallFinished { .. } => "finished".into(),
                agent_loop::AgentEvent::Done { .. } => "done".into(),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(kinds, ["started", "finished", "started", "finished", "done"]);
        assert!(matches!(
            events.last(),
            Some(agent_loop::AgentEvent::Done { answer, reason, .. })
                if answer == "final answer" && reason == "finish"
        ));

        // DB mirrors the transcript: user, asst(tool_call), tool, asst(finish), tool, asst(answer).
        let hist = load_history(&conn, chat_id, false).unwrap();
        let roles: Vec<Role> = hist.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            [Role::User, Role::Assistant, Role::Tool, Role::Assistant, Role::Tool, Role::Assistant]
        );
        assert_eq!(hist[1].tool_calls[0].name, "echo");
        assert_eq!(hist[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(hist[3].tool_calls[0].name, "finish_task");
        assert_eq!(hist[5].text_content(), Some("final answer"));
        // Thinking traces persist with their turns: the tool-call turn keeps
        // its own trace, and the finish_task answer turn carries the trace
        // accumulated over the run, so a follow-up request can replay it.
        assert_eq!(hist[1].reasoning_content.as_deref(), Some("planning the echo"));
        assert_eq!(hist[2].reasoning_content, None);
        assert_eq!(hist[5].reasoning_content.as_deref(), Some("planning the echo"));
        let dtos = chat_history(&conn, pid).unwrap();
        assert_eq!(dtos[1].reasoning.as_deref(), Some("planning the echo"));
        assert_eq!(dtos[5].reasoning.as_deref(), Some("planning the echo"));
        fs::remove_dir_all(&wd).unwrap();
    }

    // Backend whose rounds can fail, driving the after-tool LLM-failure path.
    struct FlakyBackend {
        outcomes: Mutex<VecDeque<std::result::Result<ChatOutcome, AppError>>>,
    }

    impl ChatBackend for FlakyBackend {
        fn chat_stream(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[ToolSpec]>,
            _params: &ChatParams,
            _on_event: &mut dyn FnMut(ChatEvent),
        ) -> Result<ChatOutcome> {
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(AppError::Invalid("flaky backend exhausted".into())))
        }
    }

    #[test]
    fn llm_error_after_a_successful_tool_persists_the_warning_in_the_transcript() {
        let (wd, conn, pid) = setup("chat-llm-error-after-tool");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        append_message(&conn, chat_id, Role::User, Some("go"), &[], None, None).unwrap();

        let backend = FlakyBackend {
            outcomes: Mutex::new(
                vec![
                    Ok(tools_outcome(vec![call("c1", "echo", json!({ "text": "hi" }))])),
                    Err(AppError::Http(
                        "HTTP 400: reasoning_content must be passed back".into(),
                    )),
                ]
                .into(),
            ),
        };
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Skip)));
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut on_message = |msg: &ChatMessage| {
            append_chat_message(&conn, chat_id, msg).unwrap();
        };
        let result = agent_loop::run_with(
            &mut state,
            &test_config("go"),
            &DenyAllConfirmer,
            |_| {},
            &mut on_message,
        )
        .unwrap();
        assert_eq!(result.finish_reason, "llm_error");
        persist_final_answer(&conn, chat_id, &result).unwrap();

        // The tool result and the warning both survive: user, asst(tool_call),
        // tool, asst(warning) — the completed action is never lost.
        let hist = load_history(&conn, chat_id, false).unwrap();
        let roles: Vec<Role> = hist.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
        );
        assert_eq!(hist[2].tool_call_id.as_deref(), Some("c1"));
        let warning = hist[3].text_content().unwrap();
        assert!(warning.contains("Действие выполнено, но ответ модели не получен"));
        assert!(warning.contains("HTTP 400"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn step_limit_persists_the_fallback_once_plus_the_stop_notice() {
        let (wd, conn, pid) = setup("chat-step-limit");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        let backend = ScriptedBackend::new(vec![text_outcome("thought")]);
        let mut catalog = catalog_with(None);
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut cfg = test_config("go");
        cfg.max_steps = 1;
        let mut on_message = |msg: &ChatMessage| {
            append_message(&conn, chat_id, msg.role, msg.text_content(), &msg.tool_calls, None, None)
                .unwrap();
        };
        let result = agent_loop::run_with(
            &mut state,
            &cfg,
            &DenyAllConfirmer,
            |_| {},
            &mut on_message,
        )
        .unwrap();
        persist_final_answer(&conn, chat_id, &result).unwrap();
        let hist = load_history(&conn, chat_id, false).unwrap();
        // The latched fallback is persisted once through the hook, and the
        // run ends with an explicit stop notice naming the limit.
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].text_content(), Some("thought"));
        assert_eq!(hist[1].text_content(), Some("Остановлен по лимиту шагов: 1"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn final_answer_is_persisted_exactly_once() {
        // The streamed deltas are display-only. When a round streams its answer
        // as content AND ends on finish_task with the same text (the normal
        // streaming shape), the content is latched as the fallback, not written
        // as its own turn, so persist_final_answer writes the answer exactly
        // once — no duplicate copy of the streamed text.
        let (wd, conn, pid) = setup("chat-final-once");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        append_message(&conn, chat_id, Role::User, Some("go"), &[], None, None).unwrap();

        let mut streamed_finish = tools_outcome(vec![finish_call("Привет!")]);
        streamed_finish.content = Some("Привет!".into());
        let backend = ScriptedBackend::new(vec![streamed_finish]);
        let mut catalog = catalog_with(None);
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut on_message = |msg: &ChatMessage| {
            append_chat_message(&conn, chat_id, msg).unwrap();
        };
        let result = agent_loop::run_with(
            &mut state,
            &test_config("go"),
            &DenyAllConfirmer,
            |_| {},
            &mut on_message,
        )
        .unwrap();
        persist_final_answer(&conn, chat_id, &result).unwrap();

        let hist = load_history(&conn, chat_id, false).unwrap();
        let answer_copies = hist
            .iter()
            .filter(|m| m.role == Role::Assistant && m.text_content() == Some("Привет!"))
            .count();
        assert_eq!(answer_copies, 1, "final answer must be persisted exactly once");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn cancel_flag_stops_a_run_and_keeps_completed_steps_persisted() {
        let (wd, conn, pid) = setup("chat-cancel");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        let cancel = Arc::new(AtomicBool::new(true));
        let backend = ScriptedBackend::new(vec![tools_outcome(vec![finish_call("never")])]);
        let mut catalog = catalog_with(None);
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut cfg = test_config("go");
        cfg.cancel = Some(Arc::clone(&cancel));
        let result = agent_loop::run_with(&mut state, &cfg, &AutoConfirmer::allowing(true), |_| {}, |_| {})
            .unwrap();
        assert_eq!(result.finish_reason, "cancelled");
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        persist_final_answer(&conn, chat_id, &result).unwrap();
        assert!(load_history(&conn, chat_id, false).unwrap().is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }
}
