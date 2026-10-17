// The agent loop driver (SPEC 6.4): the run ends ONLY on finish_task or the
// step limit; the final assistant text of each round is latched as a fallback
// answer; the transcript is append-only for prompt-caching friendliness.
use super::catalog::{Catalog, EnableOutcome, Resolution, ENABLE_TOOLS, LIST_TOOLS};
use super::permissions::{ConfirmDecision, ConfirmRequest, Confirmer};
use super::result::{ToolErrorCode, ToolResult};
use super::tool::{validate_args, PermissionPolicy, ToolCtx};
use crate::error::Result;
use crate::llm::{ChatEvent, ChatMessage, ChatOutcome, ChatParams, LlmClient, ToolCall, ToolSpec};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// Lets the loop run against a fake LLM in tests (no network).
pub trait ChatBackend: Send + Sync {
    fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolSpec]>,
        params: &ChatParams,
        on_event: &mut dyn FnMut(ChatEvent),
    ) -> Result<ChatOutcome>;
}

impl ChatBackend for LlmClient {
    fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolSpec]>,
        params: &ChatParams,
        on_event: &mut dyn FnMut(ChatEvent),
    ) -> Result<ChatOutcome> {
        LlmClient::chat_stream(self, messages, tools, params, on_event)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AgentEvent {
    TextDelta {
        text: String,
    },
    ToolCallStarted {
        name: String,
        args: Value,
    },
    ToolCallFinished {
        name: String,
        ok: bool,
        envelope: Value,
    },
    ConfirmationRequested {
        tool: String,
        args: Value,
        danger_reason: Option<String>,
    },
    Done {
        answer: String,
        reason: String,
    },
    Error {
        code: String,
        message: String,
    },
}

pub struct AgentRunConfig {
    // Pre-built model-facing transcript: system prompt + history (possibly
    // folded into checkpoint summaries by chat::context) + the fresh user
    // turn. The loop only appends to it, never rewrites the prefix.
    pub messages: Vec<ChatMessage>,
    pub max_steps: usize,
    // Full autopilot (SPEC 5.2): every confirmation is auto-allowed.
    pub allow_all: bool,
    pub project_id: Option<i64>,
    // Harness session of this run, when it is one (schedule tools bind to it).
    pub session_id: Option<i64>,
    pub params: ChatParams,
    // Checked between steps; set by cancel_run for this project's run.
    pub cancel: Option<Arc<AtomicBool>>,
    // Handed to every tool through ToolCtx: fs/task/memory tools borrow the
    // run thread's DB connection and working dir through it.
    pub state: Option<Arc<crate::state::AppState>>,
    pub working_dir: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRunResult {
    pub answer: String,
    // "finish" | "stop" | "step_limit"
    pub finish_reason: String,
    pub steps: usize,
}

pub struct AgentState<'a> {
    pub backend: &'a dyn ChatBackend,
    pub catalog: &'a mut Catalog,
}

pub fn run(
    state: &mut AgentState<'_>,
    config: &AgentRunConfig,
    confirmer: &dyn Confirmer,
    emit: impl FnMut(AgentEvent),
) -> Result<AgentRunResult> {
    run_with(state, config, confirmer, emit, |_| {})
}

// on_message sees every message the loop appends to the transcript, in the
// same order, before the next backend call. Chat persistence mirrors the
// transcript through this hook: the loop stays append-only and the DB copy
// cannot diverge from what the model actually saw.
pub fn run_with(
    state: &mut AgentState<'_>,
    config: &AgentRunConfig,
    confirmer: &dyn Confirmer,
    mut emit: impl FnMut(AgentEvent),
    mut on_message: impl FnMut(&ChatMessage),
) -> Result<AgentRunResult> {
    let mut messages = config.messages.clone();

    let ctx = ToolCtx {
        project_id: config.project_id,
        session_id: config.session_id,
        working_dir: config.working_dir.clone(),
        state: config.state.clone(),
    };
    let mut fallback = String::new();
    let mut always_allow: BTreeSet<String> = BTreeSet::new();

    for step in 1..=config.max_steps {
        if config
            .cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            emit(AgentEvent::Done {
                answer: fallback.clone(),
                reason: "cancelled".into(),
            });
            return Ok(AgentRunResult {
                answer: fallback,
                finish_reason: "cancelled".into(),
                steps: step - 1,
            });
        }
        let specs = state.catalog.to_specs();
        let mut on_event = |ev: ChatEvent| {
            if let ChatEvent::Delta(text) = ev {
                emit(AgentEvent::TextDelta { text });
            }
        };
        let outcome = match state.backend.chat_stream(&messages, Some(&specs), &config.params, &mut on_event) {
            Ok(outcome) => outcome,
            Err(e) => {
                emit(AgentEvent::Error {
                    code: "llm".into(),
                    message: e.to_string(),
                });
                return Err(e);
            }
        };

        // Latch this round's final text as the fallback answer candidate.
        if let Some(content) = outcome.content.as_deref().filter(|c| !c.trim().is_empty()) {
            fallback = content.to_string();
        }

        if outcome.tool_calls.is_empty() {
            if let Some(content) = outcome.content {
                let msg = ChatMessage::assistant(content);
                on_message(&msg);
                messages.push(msg);
            }
            continue;
        }
        let assistant_msg = ChatMessage::assistant_tool_calls(outcome.tool_calls.clone());
        on_message(&assistant_msg);
        messages.push(assistant_msg);

        for tc in &outcome.tool_calls {
            emit(AgentEvent::ToolCallStarted {
                name: tc.name.clone(),
                args: tc.arguments.clone(),
            });
            let envelope = execute_tool_call(
                state.catalog,
                tc,
                config,
                confirmer,
                &mut always_allow,
                &ctx,
                &mut emit,
            );
            let ok = envelope.ok;
            let envelope_json = serde_json::to_value(&envelope)?;
            emit(AgentEvent::ToolCallFinished {
                name: tc.name.clone(),
                ok,
                envelope: envelope_json.clone(),
            });
            let tool_msg = ChatMessage::tool_result(
                &tc.id,
                serde_json::to_string(&envelope_json)?,
            );
            on_message(&tool_msg);
            messages.push(tool_msg);

            if tc.name == "finish_task" && ok {
                let answer = tc
                    .arguments
                    .get("answer")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let reason = tc
                    .arguments
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("finish")
                    .to_string();
                emit(AgentEvent::Done {
                    answer: answer.clone(),
                    reason: reason.clone(),
                });
                return Ok(AgentRunResult {
                    answer,
                    finish_reason: reason,
                    steps: step,
                });
            }
        }
    }

    emit(AgentEvent::Done {
        answer: fallback.clone(),
        reason: "step_limit".into(),
    });
    Ok(AgentRunResult {
        answer: fallback,
        finish_reason: "step_limit".into(),
        steps: config.max_steps,
    })
}

fn execute_tool_call(
    catalog: &mut Catalog,
    tc: &ToolCall,
    config: &AgentRunConfig,
    confirmer: &dyn Confirmer,
    always_allow: &mut BTreeSet<String>,
    ctx: &ToolCtx,
    emit: &mut impl FnMut(AgentEvent),
) -> ToolResult {
    if let Some(err) = &tc.arguments_error {
        return ToolResult::err(ToolErrorCode::InvalidArgs, format!("arguments are not valid JSON: {err}"));
    }

    // Lazy discovery meta-tools are driven by the catalog itself, not by a
    // registered Tool object (they need mutable catalog access).
    match tc.name.as_str() {
        LIST_TOOLS => return ToolResult::ok(catalog.list_summary()),
        ENABLE_TOOLS => {
            let Some(names) = tc.arguments.get("tools").and_then(Value::as_array) else {
                return ToolResult::err(ToolErrorCode::InvalidArgs, "missing required property \"tools\"");
            };
            let mut results = Vec::new();
            for name in names {
                let Some(name) = name.as_str() else {
                    return ToolResult::err(ToolErrorCode::InvalidArgs, "tools must be an array of strings");
                };
                let status = match catalog.enable(name) {
                    EnableOutcome::Activated => "activated",
                    EnableOutcome::AlreadyActive => "already_active",
                    EnableOutcome::Hidden(reason) => {
                        results.push(json!({ "name": name, "status": "hidden", "reason": reason }));
                        continue;
                    }
                    EnableOutcome::NotFound => "not_found",
                    EnableOutcome::LimitExceeded => "limit_exceeded",
                };
                results.push(json!({ "name": name, "status": status }));
            }
            return ToolResult::ok(json!({ "results": results }));
        }
        _ => {}
    }

    let tool = match catalog.resolve(&tc.name) {
        Resolution::Found(tool) => tool.clone(),
        Resolution::Hidden(reason) => {
            return ToolResult::err(ToolErrorCode::Hidden, format!("hidden: {reason}"))
        }
        Resolution::Unknown => {
            return ToolResult::err(ToolErrorCode::UnknownTool, format!("unknown tool: {}", tc.name))
        }
    };

    if let Err(msg) = validate_args(&tool.parameters(), &tc.arguments) {
        return ToolResult::err(ToolErrorCode::InvalidArgs, msg);
    }

    let danger = tool.danger_reason(&tc.arguments, ctx);
    let needs_confirm = match tool.policy() {
        PermissionPolicy::Skip => false,
        PermissionPolicy::Check => true,
        // CheckConditional: the tool itself decides per call (SPEC 5.2,
        // e.g. fs_write only confirms outside Harness/ and Projects/).
        PermissionPolicy::CheckConditional => danger.is_some(),
    };
    if needs_confirm && !config.allow_all && !always_allow.contains(&tc.name) {
        emit(AgentEvent::ConfirmationRequested {
            tool: tc.name.clone(),
            args: tc.arguments.clone(),
            danger_reason: danger.clone(),
        });
        let req = ConfirmRequest {
            tool: tc.name.clone(),
            args: tc.arguments.clone(),
            project_id: config.project_id,
            danger_reason: danger,
        };
        match confirmer.confirm(&req) {
            ConfirmDecision::Allow => {}
            ConfirmDecision::Deny => {
                return ToolResult::err(ToolErrorCode::DeniedByUser, "user denied this tool call")
            }
            ConfirmDecision::AllowAlways => {
                always_allow.insert(tc.name.clone());
            }
        }
    }

    let result = tool
        .execute(&tc.arguments, ctx)
        .unwrap_or_else(|e| ToolResult::err(ToolErrorCode::ExecutionError, e.to_string()));
    result.capped(tool.max_result_len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::finish::FinishTaskTool;
    use crate::agent::permissions::{AutoConfirmer, DenyAllConfirmer};
    use crate::agent::tool::{EchoTool, PermissionPolicy};
    use crate::error::AppError;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    struct ScriptedBackend {
        rounds: Mutex<VecDeque<ChatOutcome>>,
        calls: AtomicUsize,
    }

    impl ScriptedBackend {
        fn new(rounds: Vec<ChatOutcome>) -> Self {
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

    struct ScriptedConfirmer {
        decisions: Mutex<VecDeque<ConfirmDecision>>,
        calls: AtomicUsize,
    }

    impl ScriptedConfirmer {
        fn new(decisions: Vec<ConfirmDecision>) -> Self {
            ScriptedConfirmer {
                decisions: Mutex::new(decisions.into()),
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl Confirmer for ScriptedConfirmer {
        fn confirm(&self, _req: &ConfirmRequest) -> ConfirmDecision {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.decisions
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(ConfirmDecision::Deny)
        }
    }

    fn text_outcome(content: &str) -> ChatOutcome {
        ChatOutcome {
            content: Some(content.into()),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
        }
    }

    fn tools_outcome(calls: Vec<ToolCall>) -> ChatOutcome {
        ChatOutcome {
            content: None,
            tool_calls: calls,
            finish_reason: Some("tool_calls".into()),
            usage: None,
        }
    }

    fn call(id: &str, name: &str, args: Value) -> ToolCall {
        ToolCall::new(id.into(), name.into(), args.to_string())
    }

    fn finish_call(answer: &str) -> ToolCall {
        call("f1", "finish_task", json!({ "action": "finish", "answer": answer }))
    }

    fn catalog_with(extra: Option<EchoTool>) -> Catalog {
        let mut c = Catalog::new();
        c.register(Arc::new(FinishTaskTool), true).unwrap();
        if let Some(tool) = extra {
            c.register(Arc::new(tool), true).unwrap();
        }
        c
    }

    fn config(max_steps: usize) -> AgentRunConfig {
        AgentRunConfig {
            messages: vec![ChatMessage::system("sys"), ChatMessage::user("go")],
            max_steps,
            allow_all: false,
            project_id: None,
            session_id: None,
            params: ChatParams {
                model: "test-model".into(),
                temperature: None,
                max_tokens: None,
            },
            cancel: None,
            state: None,
            working_dir: None,
        }
    }

    fn envelope_for(events: &[AgentEvent], name: &str) -> Value {
        events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolCallFinished { name: n, envelope, .. } if n == name => Some(envelope.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no ToolCallFinished for {name}"))
    }

    #[test]
    fn loop_ends_on_finish_task_and_returns_its_answer() {
        let backend = ScriptedBackend::new(vec![
            text_outcome("working on it"),
            tools_outcome(vec![finish_call("the answer")]),
        ]);
        let mut catalog = catalog_with(None);
        let mut state = AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let result = run(&mut state, &config(8), &AutoConfirmer::allowing(false), |e| events.push(e)).unwrap();
        assert_eq!(result.answer, "the answer");
        assert_eq!(result.finish_reason, "finish");
        assert_eq!(result.steps, 2);
        assert!(events.iter().any(|e| matches!(e, AgentEvent::TextDelta { text } if text == "working on it")));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Done { answer, reason } if answer == "the answer" && reason == "finish")));
    }

    #[test]
    fn step_limit_returns_latched_fallback() {
        let backend = ScriptedBackend::new(vec![
            text_outcome("first thought"),
            text_outcome("second thought"),
        ]);
        let mut catalog = catalog_with(None);
        let mut state = AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let result = run(&mut state, &config(2), &DenyAllConfirmer, |_| {}).unwrap();
        assert_eq!(result.answer, "second thought");
        assert_eq!(result.finish_reason, "step_limit");
        assert_eq!(result.steps, 2);
    }

    #[test]
    fn read_only_tool_runs_without_confirmation() {
        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "echo", json!({ "text": "hi" }))]),
            tools_outcome(vec![finish_call("done")]),
        ]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Skip)));
        let mut state = AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let result = run(&mut state, &config(8), &DenyAllConfirmer, |e| events.push(e)).unwrap();
        assert_eq!(result.answer, "done");
        assert!(envelope_for(&events, "echo").get("ok").unwrap().as_bool().unwrap());
    }

    #[test]
    fn check_tool_denied_yields_denied_by_user_and_loop_continues() {
        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "echo", json!({ "text": "hi" }))]),
            tools_outcome(vec![finish_call("recovered")]),
        ]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Check)));
        let mut state = AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let result = run(&mut state, &config(8), &DenyAllConfirmer, |e| events.push(e)).unwrap();
        assert_eq!(result.answer, "recovered");
        let envelope = envelope_for(&events, "echo");
        assert_eq!(envelope["error"]["code"], "denied_by_user");
        assert!(events.iter().any(|e| matches!(e, AgentEvent::ConfirmationRequested { tool, .. } if tool == "echo")));
    }

    #[test]
    fn allow_always_is_remembered_for_that_tool() {
        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "echo", json!({ "text": "a" }))]),
            tools_outcome(vec![call("c2", "echo", json!({ "text": "b" }))]),
            tools_outcome(vec![finish_call("done")]),
        ]);
        let confirmer = ScriptedConfirmer::new(vec![ConfirmDecision::AllowAlways, ConfirmDecision::Deny]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Check)));
        let mut state = AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let result = run(&mut state, &config(8), &confirmer, |e| events.push(e)).unwrap();
        assert_eq!(result.answer, "done");
        // The second call must not consult the confirmer (its scripted Deny
        // would have produced denied_by_user).
        assert_eq!(confirmer.calls.load(Ordering::SeqCst), 1);
        assert!(envelope_for(&events, "echo").get("ok").unwrap().as_bool().unwrap());
    }

    #[test]
    fn invalid_arguments_yield_invalid_args_and_loop_continues() {
        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "echo", json!({}))]),
            tools_outcome(vec![finish_call("ok")]),
        ]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Skip)));
        let mut state = AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let result = run(&mut state, &config(8), &DenyAllConfirmer, |e| events.push(e)).unwrap();
        assert_eq!(result.answer, "ok");
        assert_eq!(envelope_for(&events, "echo")["error"]["code"], "invalid_args");
    }

    #[test]
    fn unknown_tool_yields_unknown_tool_code() {
        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "nope", json!({}))]),
            tools_outcome(vec![finish_call("ok")]),
        ]);
        let mut catalog = catalog_with(None);
        let mut state = AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let result = run(&mut state, &config(8), &DenyAllConfirmer, |e| events.push(e)).unwrap();
        assert_eq!(result.answer, "ok");
        assert_eq!(envelope_for(&events, "nope")["error"]["code"], "unknown_tool");
    }

    #[test]
    fn hidden_tool_yields_hidden_code() {
        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "echo", json!({ "text": "x" }))]),
            tools_outcome(vec![finish_call("ok")]),
        ]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Skip)));
        catalog.hide("echo", "disabled for this project");
        let mut state = AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let result = run(&mut state, &config(8), &DenyAllConfirmer, |e| events.push(e)).unwrap();
        assert_eq!(result.answer, "ok");
        let envelope = envelope_for(&events, "echo");
        assert_eq!(envelope["error"]["code"], "hidden");
        assert!(envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("hidden: disabled for this project"));
    }

    #[test]
    fn result_length_cap_yields_limit_exceeded() {
        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "echo", json!({ "text": "x".repeat(500) }))]),
            tools_outcome(vec![finish_call("ok")]),
        ]);
        let mut echo = EchoTool::new(PermissionPolicy::Skip);
        echo.max_len = 100;
        let mut catalog = catalog_with(Some(echo));
        let mut state = AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let result = run(&mut state, &config(8), &DenyAllConfirmer, |e| events.push(e)).unwrap();
        assert_eq!(result.answer, "ok");
        assert_eq!(envelope_for(&events, "echo")["error"]["code"], "limit_exceeded");
    }

    #[test]
    fn allow_all_autopilot_bypasses_confirmation() {
        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "echo", json!({ "text": "hi" }))]),
            tools_outcome(vec![finish_call("done")]),
        ]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Check)));
        let mut state = AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let result = run(&mut state, &config_with_autopilot(), &DenyAllConfirmer, |e| events.push(e)).unwrap();
        assert_eq!(result.answer, "done");
        assert!(envelope_for(&events, "echo").get("ok").unwrap().as_bool().unwrap());
        assert!(!events.iter().any(|e| matches!(e, AgentEvent::ConfirmationRequested { .. })));
    }

    fn config_with_autopilot() -> AgentRunConfig {
        let mut cfg = config(8);
        cfg.allow_all = true;
        cfg
    }

    #[test]
    fn lazy_discovery_meta_tools_enable_catalog_tools() {
        struct Extra;
        impl crate::agent::tool::Tool for Extra {
            fn name(&self) -> &str {
                "extra"
            }
            fn description(&self) -> &str {
                "extra tool"
            }
            fn parameters(&self) -> Value {
                json!({ "type": "object", "properties": {}, "additionalProperties": false })
            }
            fn policy(&self) -> PermissionPolicy {
                PermissionPolicy::Skip
            }
            fn execute(&self, _args: &Value, _ctx: &ToolCtx) -> Result<ToolResult> {
                Ok(ToolResult::ok(json!({"ran": true})))
            }
        }

        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![
                call("d1", LIST_TOOLS, json!({})),
                call("d2", ENABLE_TOOLS, json!({ "tools": ["extra", "ghost"] })),
            ]),
            tools_outcome(vec![call("c1", "extra", json!({}))]),
            tools_outcome(vec![finish_call("ok")]),
        ]);
        let mut catalog = Catalog::new();
        catalog.register(Arc::new(FinishTaskTool), true).unwrap();
        catalog.register(Arc::new(Extra), false).unwrap();
        let mut state = AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let result = run(&mut state, &config(8), &DenyAllConfirmer, |e| events.push(e)).unwrap();
        assert_eq!(result.answer, "ok");
        let list = envelope_for(&events, LIST_TOOLS);
        assert_eq!(list["data"]["available"], json!(["extra"]));
        let enable = envelope_for(&events, ENABLE_TOOLS);
        assert_eq!(enable["data"]["results"][0]["status"], "activated");
        assert_eq!(enable["data"]["results"][1]["status"], "not_found");
        assert!(envelope_for(&events, "extra").get("ok").unwrap().as_bool().unwrap());
    }

    #[test]
    fn transcript_is_append_only_and_tool_errors_do_not_crash_the_loop() {
        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "echo", json!({ "bogus": 1 }))]),
            tools_outcome(vec![finish_call("ok")]),
        ]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Skip)));
        let mut state = AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let result = run(&mut state, &config(8), &DenyAllConfirmer, |_| {}).unwrap();
        assert_eq!(result.answer, "ok");
        // The backend saw: system+user, then +assistant/tool, i.e. growth only.
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn cancel_flag_stops_the_loop_between_steps() {
        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "echo", json!({ "text": "a" }))]),
            tools_outcome(vec![finish_call("never")]),
        ]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Skip)));
        let mut state = AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let mut cfg = config(8);
        cfg.cancel = Some(Arc::clone(&cancel));
        // Flip the flag when the first tool result is appended: the loop must
        // stop before the second backend round.
        let flag = Arc::clone(&cancel);
        let result = run_with(&mut state, &cfg, &DenyAllConfirmer, |_| {}, move |m| {
            if m.role == crate::llm::Role::Tool {
                flag.store(true, Ordering::Relaxed);
            }
        })
        .unwrap();
        assert_eq!(result.finish_reason, "cancelled");
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    }
}
