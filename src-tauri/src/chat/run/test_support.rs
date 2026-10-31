// Shared test scaffolding for the run submodules: a scripted ChatBackend and
// the outcome/config/catalog builders the loop-driving tests in `bridge` and
// `persist` both need. Compiled only under `cfg(test)`.
use crate::agent::catalog::Catalog;
use crate::agent::finish::FinishTaskTool;
use crate::agent::r#loop as agent_loop;
use crate::agent::r#loop::ChatBackend;
use crate::agent::tool::EchoTool;
use crate::chat::test_support::call;
use crate::error::{AppError, Result};
use crate::llm::{ChatEvent, ChatMessage, ChatOutcome, ChatParams, ToolCall, ToolSpec};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) struct ScriptedBackend {
    rounds: Mutex<VecDeque<ChatOutcome>>,
    pub(crate) calls: AtomicUsize,
}

impl ScriptedBackend {
    pub(crate) fn new(rounds: Vec<ChatOutcome>) -> Self {
        ScriptedBackend {
            rounds: Mutex::new(rounds.into()),
            calls: AtomicUsize::new(0),
        }
    }
}

impl ChatBackend for ScriptedBackend {
    fn chat_stream(
        &self,
        _messages: &[ChatMessage],
        _tools: Option<&[ToolSpec]>,
        _params: &ChatParams,
        on_event: &mut dyn FnMut(ChatEvent),
    ) -> Result<ChatOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let outcome = self
            .rounds
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| AppError::Invalid("scripted backend exhausted".into()))?;
        if let Some(content) = &outcome.content {
            on_event(ChatEvent::Delta(content.clone()));
        }
        Ok(outcome)
    }
}

pub(crate) fn tools_outcome(calls: Vec<ToolCall>) -> ChatOutcome {
    ChatOutcome {
        content: None,
        reasoning: None,
        tool_calls: calls,
        finish_reason: Some("tool_calls".into()),
        usage: None,
    }
}

pub(crate) fn finish_call(answer: &str) -> ToolCall {
    call("f1", "finish_task", json!({ "action": "finish", "answer": answer }))
}

pub(crate) fn test_config(user_message: &str) -> agent_loop::AgentRunConfig {
    agent_loop::AgentRunConfig {
        scope: None,
        messages: vec![ChatMessage::system("sys"), ChatMessage::user(user_message)],
        max_steps: 8,
        limits: agent_loop::RunLimits::default(),
        allow_all: false,
        project_id: None,
        session_id: None,
        chat_id: None,
        params: ChatParams {
            model: "test-model".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            requires_reasoning_passback: false,
        },
        cancel: None,
        state: None,
        working_dir: None,
        notes_root: None,
    }
}

pub(crate) fn catalog_with(extra: Option<EchoTool>) -> Catalog {
    let mut c = Catalog::new();
    c.register(Arc::new(FinishTaskTool), true).unwrap();
    if let Some(tool) = extra {
        c.register(Arc::new(tool), true).unwrap();
    }
    c
}
