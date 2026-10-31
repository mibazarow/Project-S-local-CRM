// Integration + unit tests for the agent loop. Kept as one module (the loop is
// tested end-to-end through run/run_with/run_inner); the handful of extracted
// step units at the bottom poke Run internals directly.
use super::step::{
    Run, FALSE_SUCCESS_NUDGE, LLM_AFTER_TOOL_WARNING, LOOP_NUDGE, LOOP_REPEAT_LIMIT,
    NO_ATTEMPT_NUDGE,
};
use super::{
    run, run_inner, run_with, AgentEvent, AgentRunConfig, AgentState, ChatBackend, RunLimits,
};
use crate::agent::catalog::{Catalog, ENABLE_TOOLS, LIST_TOOLS};
use crate::agent::finish::FinishTaskTool;
use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::{EchoTool, PermissionPolicy, Tool, ToolCtx};
use crate::confirm::{AutoConfirmer, ConfirmDecision, ConfirmRequest, Confirmer, DenyAllConfirmer};
use crate::error::AppError;
use crate::error::Result;
use crate::llm::{ChatEvent, ChatMessage, ChatOutcome, ChatParams, Role, ToolCall, ToolSpec};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct ScriptedBackend {
    rounds: Mutex<VecDeque<ChatOutcome>>,
    calls: AtomicUsize,
    // Every message list the backend was called with, for guard assertions.
    seen: Mutex<Vec<Vec<ChatMessage>>>,
}

impl ScriptedBackend {
    fn new(rounds: Vec<ChatOutcome>) -> Self {
        ScriptedBackend {
            rounds: Mutex::new(rounds.into()),
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        }
    }

    // How many copies of the nudge sit in the final transcript the model
    // was last called with (the transcript is cumulative, so the last
    // round reflects every nudge the loop ever pushed).
    fn nudge_count(&self, nudge: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .last()
            .map(|round| {
                round
                    .iter()
                    .filter(|m| m.role == Role::System && m.text_content() == Some(nudge))
                    .count()
            })
            .unwrap_or(0)
    }
}

impl ChatBackend for ScriptedBackend {
    fn chat_stream(
        &self,
        messages: &[ChatMessage],
        _tools: Option<&[ToolSpec]>,
        _params: &ChatParams,
        on_event: &mut dyn FnMut(ChatEvent),
    ) -> Result<ChatOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(messages.to_vec());
        let outcome = self
            .rounds
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| AppError::Invalid("scripted backend exhausted".into()))?;
        if let Some(reasoning) = &outcome.reasoning {
            on_event(ChatEvent::ReasoningDelta(reasoning.clone()));
        }
        if let Some(content) = &outcome.content {
            on_event(ChatEvent::Delta(content.clone()));
        }
        Ok(outcome)
    }
}

// Backend whose rounds can fail with an AppError, to drive the
// after-tool LLM-failure path.
struct FlakyBackend {
    outcomes: Mutex<VecDeque<std::result::Result<ChatOutcome, AppError>>>,
    calls: AtomicUsize,
}

impl FlakyBackend {
    fn new(outcomes: Vec<std::result::Result<ChatOutcome, AppError>>) -> Self {
        FlakyBackend {
            outcomes: Mutex::new(outcomes.into()),
            calls: AtomicUsize::new(0),
        }
    }
}

impl ChatBackend for FlakyBackend {
    fn chat_stream(
        &self,
        _messages: &[ChatMessage],
        _tools: Option<&[ToolSpec]>,
        _params: &ChatParams,
        _on_event: &mut dyn FnMut(ChatEvent),
    ) -> Result<ChatOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(AppError::Invalid("flaky backend exhausted".into())))
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
        reasoning: None,
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
    }
}

fn reasoning_outcome(content: &str, reasoning: &str) -> ChatOutcome {
    ChatOutcome {
        content: Some(content.into()),
        reasoning: Some(reasoning.into()),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
    }
}

fn tools_outcome(calls: Vec<ToolCall>) -> ChatOutcome {
    ChatOutcome {
        content: None,
        reasoning: None,
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
        scope: None,
        messages: vec![ChatMessage::system("sys"), ChatMessage::user("go")],
        max_steps,
        limits: RunLimits::default(),
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

fn envelope_for(events: &[AgentEvent], name: &str) -> Value {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolCallFinished { name: n, envelope, .. } if n == name => Some(envelope.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no ToolCallFinished for {name}"))
}

// --- extracted step units (S3.4 decomposition) ---

fn probe_run() -> Run {
    Run::new(&config(8), None)
}

#[test]
fn finish_latch_fires_only_on_a_successful_finish_task_call() {
    let run = probe_run();
    // Not finish_task: no latch regardless of ok.
    let echo = call("c1", "echo", json!({ "answer": "x" }));
    assert_eq!(run.finish_latch(&echo, true), None);
    // finish_task but the call failed: the run continues.
    let failed = finish_call("nope");
    assert_eq!(run.finish_latch(&failed, false), None);
    // Successful finish_task: answer/action travel through.
    let ok = finish_call("yes");
    assert_eq!(
        run.finish_latch(&ok, true),
        Some(("yes".to_string(), "finish".to_string()))
    );
    // Missing args fall back exactly as before: empty answer, "finish" action.
    let bare = call("f2", "finish_task", json!({}));
    assert_eq!(
        run.finish_latch(&bare, true),
        Some((String::new(), "finish".to_string()))
    );
    let stop = call("f3", "finish_task", json!({ "action": "stop" }));
    assert_eq!(
        run.finish_latch(&stop, true),
        Some((String::new(), "stop".to_string()))
    );
}

#[test]
fn note_round_outcome_latches_the_last_non_blank_text_and_trims_blank_traces() {
    let mut run = probe_run();
    run.note_round_outcome(&text_outcome("first"));
    // Blank/whitespace content must not clobber the latched fallback.
    let blank = ChatOutcome {
        content: Some("   ".into()),
        reasoning: Some("  ".into()),
        tool_calls: vec![],
        finish_reason: None,
        usage: None,
    };
    run.note_round_outcome(&blank);
    assert_eq!(run.fallback, "first");
    assert_eq!(run.reasoning, None);
    run.note_round_outcome(&text_outcome("second"));
    assert_eq!(run.fallback, "second");
    // Non-blank traces accumulate across rounds, newline-joined.
    run.note_round_outcome(&reasoning_outcome("third", "trace a"));
    run.note_round_outcome(&reasoning_outcome("fourth", "trace b"));
    assert_eq!(run.reasoning.as_deref(), Some("trace a\ntrace b"));
    assert_eq!(run.fallback, "fourth");
}

#[test]
fn loop_guard_records_the_streak_but_nudges_only_at_the_limit() {
    let mut run = probe_run();
    let tc = call("c1", "echo", json!({ "text": "x" }));
    let envelope = json!({ "ok": true, "data": { "echo": "x" } });
    for _ in 1..LOOP_REPEAT_LIMIT {
        run.note_tool_call(&tc, &envelope, true);
        run.handle_loop_guard();
    }
    assert!(!run.guards.loop_nudged);
    assert!(run.messages.iter().all(|m| m.text_content() != Some(LOOP_NUDGE)));
    run.note_tool_call(&tc, &envelope, true);
    run.handle_loop_guard();
    assert!(run.guards.loop_nudged);
    assert!(run
        .messages
        .iter()
        .any(|m| m.role == Role::System && m.text_content() == Some(LOOP_NUDGE)));
    // Verifying evidence: meta-tools do not count, failed calls do not,
    // a successful real tool does (fresh run: the streak above already
    // set the flag).
    let mut run = probe_run();
    run.note_tool_call(&call("d1", LIST_TOOLS, json!({})), &json!({ "ok": true }), true);
    assert!(!run.guards.verifying_ok);
    run.note_tool_call(&tc, &envelope, false);
    assert!(!run.guards.verifying_ok);
    run.note_tool_call(&tc, &envelope, true);
    assert!(run.guards.verifying_ok);
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
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done { answer, reason, .. } if answer == "the answer" && reason == "finish")));
}

#[test]
fn streamed_content_deltas_become_ordered_text_delta_events() {
    // Progressive display: a round that streams its answer in several
    // content chunks must surface one TextDelta per chunk, in order, before
    // the run ends. The finish_task answer still travels through Done.
    struct StreamingBackend;
    impl ChatBackend for StreamingBackend {
        fn chat_stream(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[ToolSpec]>,
            _params: &ChatParams,
            on_event: &mut dyn FnMut(ChatEvent),
        ) -> Result<ChatOutcome> {
            for chunk in ["При", "вет", "! "] {
                on_event(ChatEvent::Delta(chunk.into()));
            }
            Ok(tools_outcome(vec![finish_call("Привет!")]))
        }
    }
    let backend = StreamingBackend;
    let mut catalog = catalog_with(None);
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    let mut events = Vec::new();
    let result = run(&mut state, &config(8), &AutoConfirmer::allowing(false), |e| events.push(e)).unwrap();
    let deltas: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, ["При", "вет", "! "]);
    assert_eq!(result.answer, "Привет!");
    // The Done event is the last event and carries the final answer once.
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done { answer, .. }) if answer == "Привет!"
    ));
}

#[test]
fn reasoning_streams_accumulates_and_travels_back_to_the_model() {
    let backend = ScriptedBackend::new(vec![
        reasoning_outcome("working on it", "first thought"),
        tools_outcome(vec![finish_call("the answer")]),
    ]);
    let mut catalog = catalog_with(None);
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    let mut events = Vec::new();
    let result = run(&mut state, &config(8), &AutoConfirmer::allowing(false), |e| events.push(e)).unwrap();
    // Live stream: the trace reaches the UI as its own event kind.
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::ReasoningDelta { text } if text == "first thought")));
    // Final result and the Done event carry the accumulated trace.
    assert_eq!(result.reasoning.as_deref(), Some("first thought"));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done { reasoning: Some(r), .. }) if r == "first thought"
    ));
    // The next backend call saw the assistant turn replay its trace, which
    // is what thinking-mode providers demand of a follow-up request.
    let last_round = backend.seen.lock().unwrap().last().unwrap().clone();
    let assistant = last_round
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("assistant turn in the replayed transcript");
    assert_eq!(assistant.reasoning_content.as_deref(), Some("first thought"));
    let wire = serde_json::to_value(&last_round).unwrap();
    assert_eq!(
        wire[2]["reasoning_content"].as_str(),
        Some("first thought"),
        "request body must send reasoning_content back"
    );
}

#[test]
fn step_limit_ends_with_a_notice_naming_the_limit() {
    let backend = ScriptedBackend::new(vec![
        text_outcome("first thought"),
        text_outcome("second thought"),
    ]);
    let mut catalog = catalog_with(None);
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    let mut events = Vec::new();
    let result = run(&mut state, &config(2), &DenyAllConfirmer, |e| events.push(e)).unwrap();
    assert_eq!(result.answer, "Остановлен по лимиту шагов: 2");
    assert_eq!(result.finish_reason, "step_limit");
    assert_eq!(result.steps, 2);
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done { answer, reason, .. })
            if answer == "Остановлен по лимиту шагов: 2" && reason == "step_limit"
    ));
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

// Table-driven policy resolution (SPEC 5.2): which policies consult the
// confirmer and what danger_reason the ConfirmationRequested event carries.
// Skip never confirms even when the tool reports danger; Check always does;
// CheckConditional confirms exactly when danger_reason is Some.
struct PolicyProbeTool {
    policy: PermissionPolicy,
    danger: Option<String>,
}

impl Tool for PolicyProbeTool {
    fn name(&self) -> &str {
        "probe"
    }
    fn description(&self) -> &str {
        "reports a configurable danger reason (test-only tool)"
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }
    fn policy(&self) -> PermissionPolicy {
        self.policy
    }
    fn danger_reason(&self, _args: &Value, _ctx: &ToolCtx) -> Option<String> {
        self.danger.clone()
    }
    fn execute(&self, _args: &Value, _ctx: &ToolCtx) -> Result<ToolResult> {
        Ok(ToolResult::ok(json!({ "ran": true })))
    }
}

#[test]
fn policy_resolution_matrix_decides_confirmation_and_danger_reason() {
    struct Case {
        label: &'static str,
        policy: PermissionPolicy,
        danger: Option<&'static str>,
        expect_confirm: bool,
        expect_reason: Option<&'static str>,
    }
    use PermissionPolicy::*;
    let cases = [
        Case { label: "skip-no-danger", policy: Skip, danger: None, expect_confirm: false, expect_reason: None },
        Case { label: "skip-ignores-danger", policy: Skip, danger: Some("ignored"), expect_confirm: false, expect_reason: None },
        Case { label: "check-no-reason", policy: Check, danger: None, expect_confirm: true, expect_reason: None },
        Case { label: "check-with-reason", policy: Check, danger: Some("always mutates"), expect_confirm: true, expect_reason: Some("always mutates") },
        Case { label: "conditional-no-danger", policy: CheckConditional, danger: None, expect_confirm: false, expect_reason: None },
        Case { label: "conditional-with-danger", policy: CheckConditional, danger: Some("outside territory"), expect_confirm: true, expect_reason: Some("outside territory") },
    ];
    for case in cases {
        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "probe", json!({}))]),
            tools_outcome(vec![finish_call("done")]),
        ]);
        let probe = PolicyProbeTool {
            policy: case.policy,
            danger: case.danger.map(str::to_string),
        };
        let mut catalog = catalog_with(None);
        catalog.register(Arc::new(probe), true).unwrap();
        let mut state = AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        // DenyAllConfirmer: a consulted confirmation surfaces as
        // denied_by_user, an uncalled one lets the probe run.
        let result = run(&mut state, &config(8), &DenyAllConfirmer, |e| events.push(e)).unwrap();
        assert_eq!(result.answer, "done", "{}", case.label);
        let requested = events
            .iter()
            .find(|e| matches!(e, AgentEvent::ConfirmationRequested { tool, .. } if tool == "probe"));
        assert_eq!(requested.is_some(), case.expect_confirm, "{}", case.label);
        if let Some(AgentEvent::ConfirmationRequested { danger_reason, .. }) = requested {
            assert_eq!(danger_reason.as_deref(), case.expect_reason, "{}", case.label);
        }
        let envelope = envelope_for(&events, "probe");
        if case.expect_confirm {
            assert_eq!(envelope["error"]["code"], "denied_by_user", "{}", case.label);
        } else {
            assert_eq!(envelope["ok"], json!(true), "{}", case.label);
        }
    }
}

// Wire-level check: every assistant tool_calls turn is followed by a tool
// message per tool_call_id before the next assistant turn (or the end).
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

#[test]
fn denied_tool_still_sends_a_valid_next_request() {
    let backend = ScriptedBackend::new(vec![
        tools_outcome(vec![
            call("c1", "echo", json!({ "text": "a" })),
            call("c2", "echo", json!({ "text": "b" })),
        ]),
        tools_outcome(vec![finish_call("recovered")]),
    ]);
    let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Check)));
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    let result = run(&mut state, &config(8), &DenyAllConfirmer, |_| {}).unwrap();
    assert_eq!(result.answer, "recovered");
    // The second request must answer every tool_call of the first round,
    // the denied ones included (denied_by_user tool messages): an open
    // group here is exactly the HTTP 400 this fix prevents.
    let second = backend.seen.lock().unwrap()[1].clone();
    assert!(tool_groups_are_closed(&second), "next request has an open tool_calls group");
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
fn weak_scalar_arguments_are_coerced_before_validation() {
    // EchoTool declares text as string: a weak model sending the number
    // 42 must be coerced to "42" and the call must succeed.
    let backend = ScriptedBackend::new(vec![
        tools_outcome(vec![call("c1", "echo", json!({ "text": 42 }))]),
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
    let envelope = envelope_for(&events, "echo");
    assert!(envelope.get("ok").unwrap().as_bool().unwrap());
    assert_eq!(envelope["data"]["echo"], "42");
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
fn llm_failure_after_a_tool_call_ends_the_run_with_a_warning_not_an_error() {
    let backend = FlakyBackend::new(vec![
        Ok(tools_outcome(vec![call("c1", "echo", json!({ "text": "hi" }))])),
        Err(AppError::Http(
            "HTTP 400: {\"error\":{\"message\":\"The `reasoning_content` in the thinking mode must be passed back to the API.\"}}".into(),
        )),
    ]);
    let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Skip)));
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    let mut events = Vec::new();
    let mut persisted: Vec<ChatMessage> = Vec::new();
    let result = run_with(
        &mut state,
        &config(8),
        &DenyAllConfirmer,
        |e| events.push(e),
        |m| persisted.push(m.clone()),
    )
    .unwrap();
    // Partial success, not a hard error: the run returns Ok with the
    // warning as the final answer.
    assert_eq!(result.finish_reason, "llm_error");
    assert!(result.answer.contains(LLM_AFTER_TOOL_WARNING));
    assert!(result.answer.contains("HTTP 400"));
    // The successful tool call landed in the transcript before the failure.
    assert!(envelope_for(&events, "echo")["ok"].as_bool().unwrap());
    assert!(persisted.iter().any(|m| m.role == Role::Tool));
    // The operation is not presented as failed: no Error event anywhere,
    // Done closes the run with the warning answer.
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done { reason, answer, .. })
            if reason == "llm_error" && answer.contains(LLM_AFTER_TOOL_WARNING)
    ));
    assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn llm_failure_before_any_tool_call_still_fails_the_run() {
    let backend = FlakyBackend::new(vec![Err(AppError::Http("HTTP 500: upstream down".into()))]);
    let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Skip)));
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    let mut events = Vec::new();
    let err = run(&mut state, &config(8), &DenyAllConfirmer, |e| events.push(e)).unwrap_err();
    assert!(err.to_string().contains("500"));
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Error { message, .. } if message.contains("500"))));
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

#[test]
fn cancel_during_streaming_ends_the_run_as_cancelled_not_error() {
    // The flag flips mid-stream: the backend stops streaming (as the real
    // SSE reader does) and the loop must map the partial round to the
    // "cancelled" finish reason, never to an Error event.
    struct CancellingStreamingBackend {
        cancel: Arc<AtomicBool>,
        calls: AtomicUsize,
    }
    impl ChatBackend for CancellingStreamingBackend {
        fn chat_stream(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[ToolSpec]>,
            _params: &ChatParams,
            _on_event: &mut dyn FnMut(ChatEvent),
        ) -> Result<ChatOutcome> {
            unreachable!("the loop must use the cancellable variant")
        }
        fn chat_stream_cancellable(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[ToolSpec]>,
            _params: &ChatParams,
            on_event: &mut dyn FnMut(ChatEvent),
            cancel: Option<&AtomicBool>,
        ) -> Result<ChatOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut outcome = tools_outcome(vec![finish_call("never")]);
            for chunk in ["Первая ", "половина"] {
                if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                    // Partial round: the reader closed on cancel, the
                    // accumulated text is all the round ever produced.
                    outcome.content = Some("Первая ".into());
                    outcome.tool_calls = vec![];
                    return Ok(outcome);
                }
                on_event(ChatEvent::Delta(chunk.into()));
                if chunk == "Первая " {
                    self.cancel.store(true, Ordering::Relaxed);
                }
            }
            Ok(outcome)
        }
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let backend = CancellingStreamingBackend {
        cancel: Arc::clone(&cancel),
        calls: AtomicUsize::new(0),
    };
    let mut catalog = catalog_with(None);
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    let mut cfg = config(8);
    cfg.cancel = Some(Arc::clone(&cancel));
    let mut events = Vec::new();
    let result = run(&mut state, &cfg, &DenyAllConfirmer, |e| events.push(e)).unwrap();
    assert_eq!(result.finish_reason, "cancelled");
    // The partial streamed text is latched as the answer, and the run's
    // tool calls never executed.
    assert_eq!(result.answer, "Первая ");
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::ToolCallStarted { .. })));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done { reason, .. }) if reason == "cancelled"
    ));
}

#[test]
fn cancel_after_a_tool_call_skips_the_remaining_calls_of_the_round() {
    // The flag flips inside the first tool call: the round's second call
    // must not run and the run must end as "cancelled".
    struct CancellingTool;
    impl Tool for CancellingTool {
        fn name(&self) -> &str {
            "canceller"
        }
        fn description(&self) -> &str {
            "sets the run cancel flag (test-only tool)"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {}, "additionalProperties": false })
        }
        fn policy(&self) -> PermissionPolicy {
            PermissionPolicy::Skip
        }
        fn execute(&self, _args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
            if let Some(flag) = &ctx.cancel {
                flag.store(true, Ordering::Relaxed);
            }
            Ok(ToolResult::ok(json!({ "ran": true })))
        }
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let backend = ScriptedBackend::new(vec![tools_outcome(vec![
        call("c1", "canceller", json!({})),
        call("c2", "echo", json!({ "text": "never" })),
    ])]);
    let mut catalog = Catalog::new();
    catalog.register(Arc::new(FinishTaskTool), true).unwrap();
    catalog.register(Arc::new(CancellingTool), true).unwrap();
    catalog
        .register(Arc::new(EchoTool::new(PermissionPolicy::Skip)), true)
        .unwrap();
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    let mut cfg = config(8);
    cfg.cancel = Some(Arc::clone(&cancel));
    let mut events = Vec::new();
    let result = run(&mut state, &cfg, &DenyAllConfirmer, |e| events.push(e)).unwrap();
    assert_eq!(result.finish_reason, "cancelled");
    let started: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallStarted { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(started, ["canceller"]);
}

#[test]
fn cancel_flag_is_per_run_and_never_leaks_into_the_next_run() {
    // Run one is cancelled by its own flag; run two gets a fresh flag and
    // must complete normally even though the first flag is now true.
    let first = Arc::new(AtomicBool::new(false));
    let backend = ScriptedBackend::new(vec![
        tools_outcome(vec![call("c1", "echo", json!({ "text": "a" }))]),
        text_outcome("second run"),
        tools_outcome(vec![finish_call("done")]),
    ]);
    let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Skip)));
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    let mut cfg = config(8);
    cfg.cancel = Some(Arc::clone(&first));
    let flag = Arc::clone(&first);
    let result = run_with(&mut state, &cfg, &DenyAllConfirmer, |_| {}, move |m| {
        if m.role == crate::llm::Role::Tool {
            flag.store(true, Ordering::Relaxed);
        }
    })
    .unwrap();
    assert_eq!(result.finish_reason, "cancelled");
    assert!(first.load(Ordering::Relaxed));

    let mut cfg2 = config(8);
    cfg2.cancel = Some(Arc::new(AtomicBool::new(false)));
    let result = run(&mut state, &cfg2, &DenyAllConfirmer, |_| {}).unwrap();
    assert_eq!(result.finish_reason, "finish");
    assert_eq!(result.answer, "done");
}

// --- harness reliability guards ---

#[test]
fn false_success_nudge_fires_once_without_verifying_tool() {
    let backend = ScriptedBackend::new(vec![
        text_outcome("Готово!"),
        text_outcome("Done!"),
        tools_outcome(vec![finish_call("ok")]),
    ]);
    let mut catalog = catalog_with(None);
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    let result = run(&mut state, &config(8), &DenyAllConfirmer, |_| {}).unwrap();
    assert_eq!(result.answer, "ok");
    assert_eq!(backend.nudge_count(FALSE_SUCCESS_NUDGE), 1);
}

#[test]
fn false_success_nudge_not_fired_after_successful_tool() {
    let backend = ScriptedBackend::new(vec![
        tools_outcome(vec![call("c1", "echo", json!({ "text": "hi" }))]),
        text_outcome("Готово!"),
        tools_outcome(vec![finish_call("ok")]),
    ]);
    let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Skip)));
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    let result = run(&mut state, &config(8), &DenyAllConfirmer, |_| {}).unwrap();
    assert_eq!(result.answer, "ok");
    assert_eq!(backend.nudge_count(FALSE_SUCCESS_NUDGE), 0);
}

#[test]
fn guards_do_not_fire_when_finish_task_ends_the_run() {
    let backend = ScriptedBackend::new(vec![tools_outcome(vec![finish_call("done")])]);
    let mut catalog = catalog_with(None);
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    let result = run(&mut state, &config(8), &DenyAllConfirmer, |_| {}).unwrap();
    assert_eq!(result.finish_reason, "finish");
    assert_eq!(backend.nudge_count(FALSE_SUCCESS_NUDGE), 0);
    assert_eq!(backend.nudge_count(NO_ATTEMPT_NUDGE), 0);
}

#[test]
fn loop_nudge_fires_on_three_identical_call_results() {
    let backend = ScriptedBackend::new(vec![
        tools_outcome(vec![call("c1", "echo", json!({ "text": "x" }))]),
        tools_outcome(vec![call("c2", "echo", json!({ "text": "x" }))]),
        tools_outcome(vec![call("c3", "echo", json!({ "text": "x" }))]),
        tools_outcome(vec![finish_call("ok")]),
    ]);
    let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Skip)));
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    let result = run(&mut state, &config(8), &DenyAllConfirmer, |_| {}).unwrap();
    assert_eq!(result.answer, "ok");
    assert_eq!(backend.nudge_count(LOOP_NUDGE), 1);
}

#[test]
fn loop_nudge_not_fired_when_results_change() {
    let backend = ScriptedBackend::new(vec![
        tools_outcome(vec![call("c1", "echo", json!({ "text": "a" }))]),
        tools_outcome(vec![call("c2", "echo", json!({ "text": "b" }))]),
        tools_outcome(vec![call("c3", "echo", json!({ "text": "c" }))]),
        tools_outcome(vec![finish_call("ok")]),
    ]);
    let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Skip)));
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    let result = run(&mut state, &config(8), &DenyAllConfirmer, |_| {}).unwrap();
    assert_eq!(result.answer, "ok");
    assert_eq!(backend.nudge_count(LOOP_NUDGE), 0);
}

#[test]
fn no_attempt_nudge_fires_only_with_zero_tool_calls() {
    let backend = ScriptedBackend::new(vec![
        text_outcome("I can't do that."),
        tools_outcome(vec![finish_call("ok")]),
    ]);
    let mut catalog = catalog_with(None);
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    let result = run(&mut state, &config(8), &DenyAllConfirmer, |_| {}).unwrap();
    assert_eq!(result.answer, "ok");
    assert_eq!(backend.nudge_count(NO_ATTEMPT_NUDGE), 1);

    // A run that used any tool never gets the nudge.
    let backend = ScriptedBackend::new(vec![
        tools_outcome(vec![call("c1", "echo", json!({ "text": "hi" }))]),
        text_outcome("I cannot continue."),
        tools_outcome(vec![finish_call("ok")]),
    ]);
    let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Skip)));
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    let result = run(&mut state, &config(8), &DenyAllConfirmer, |_| {}).unwrap();
    assert_eq!(result.answer, "ok");
    assert_eq!(backend.nudge_count(NO_ATTEMPT_NUDGE), 0);
}

// --- run watchdog ---

fn fake_clock(ms: &Arc<AtomicU64>) -> Box<dyn Fn() -> Duration + Send> {
    let ms = Arc::clone(ms);
    Box::new(move || Duration::from_millis(ms.load(Ordering::SeqCst)))
}

#[test]
fn watchdog_stall_stops_the_run_with_the_latched_fallback() {
    let backend = ScriptedBackend::new(vec![text_outcome("thinking"), text_outcome("never")]);
    let mut catalog = catalog_with(None);
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    // The fake clock jumps past the 10-minute stall timeout when the first
    // round's message is appended; the next step boundary must stop the run.
    let clock_ms = Arc::new(AtomicU64::new(0));
    let advance = Arc::clone(&clock_ms);
    let result = run_inner(
        &mut state,
        &config(8),
        &DenyAllConfirmer,
        |_| {},
        move |_| advance.store(11 * 60 * 1000, Ordering::SeqCst),
        Some(fake_clock(&clock_ms)),
    )
    .unwrap();
    assert_eq!(result.finish_reason, "stalled");
    assert_eq!(result.answer, "thinking");
    assert_eq!(result.steps, 1);
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn watchdog_hard_cap_stops_the_run_as_timeout() {
    let backend = ScriptedBackend::new(vec![text_outcome("thinking"), text_outcome("never")]);
    let mut catalog = catalog_with(None);
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    // Jump past the 30-minute hard cap: timeout wins over the stall check.
    let clock_ms = Arc::new(AtomicU64::new(0));
    let advance = Arc::clone(&clock_ms);
    let result = run_inner(
        &mut state,
        &config(8),
        &DenyAllConfirmer,
        |_| {},
        move |_| advance.store(31 * 60 * 1000, Ordering::SeqCst),
        Some(fake_clock(&clock_ms)),
    )
    .unwrap();
    assert_eq!(result.finish_reason, "timeout");
    assert_eq!(result.answer, "thinking");
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
}

// --- per-tool timeout ---

struct SlowTool;

impl Tool for SlowTool {
    fn name(&self) -> &str {
        "slow"
    }
    fn description(&self) -> &str {
        "sleeps far past the test timeout (test-only tool)"
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, _args: &Value, _ctx: &ToolCtx) -> Result<ToolResult> {
        std::thread::sleep(Duration::from_millis(200));
        Ok(ToolResult::ok(json!({ "late": true })))
    }
}

#[test]
fn tool_timeout_yields_execution_error_and_loop_continues() {
    let backend = ScriptedBackend::new(vec![
        tools_outcome(vec![call("c1", "slow", json!({}))]),
        tools_outcome(vec![finish_call("recovered")]),
    ]);
    let mut catalog = Catalog::new();
    catalog.register(Arc::new(FinishTaskTool), true).unwrap();
    catalog.register(Arc::new(SlowTool), true).unwrap();
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    let mut cfg = config(8);
    cfg.limits.tool_timeout = Duration::from_millis(50);
    let mut events = Vec::new();
    let result = run(&mut state, &cfg, &DenyAllConfirmer, |e| events.push(e)).unwrap();
    assert_eq!(result.answer, "recovered");
    let envelope = envelope_for(&events, "slow");
    assert_eq!(envelope["error"]["code"], "execution_error");
    assert!(envelope["error"]["message"]
        .as_str()
        .unwrap()
        .contains("tool timed out after 0.05s"));
}

// A tool that never looks at the cancel flag: the run must still stop
// promptly, so the wait for the worker is what observes the stop request.
struct HangingTool;

impl Tool for HangingTool {
    fn name(&self) -> &str {
        "hanger"
    }
    fn description(&self) -> &str {
        "sets the run cancel flag and then hangs (test-only tool)"
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, _args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        if let Some(flag) = &ctx.cancel {
            flag.store(true, Ordering::Relaxed);
        }
        std::thread::sleep(Duration::from_secs(5));
        Ok(ToolResult::ok(json!({ "late": true })))
    }
}

#[test]
fn stop_while_a_tool_runs_does_not_wait_out_the_tool_timeout() {
    let cancel = Arc::new(AtomicBool::new(false));
    let backend = ScriptedBackend::new(vec![tools_outcome(vec![call("c1", "hanger", json!({}))])]);
    let mut catalog = Catalog::new();
    catalog.register(Arc::new(FinishTaskTool), true).unwrap();
    catalog.register(Arc::new(HangingTool), true).unwrap();
    let mut state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    let mut cfg = config(8);
    cfg.limits.tool_timeout = Duration::from_secs(30);
    cfg.cancel = Some(Arc::clone(&cancel));
    let started = std::time::Instant::now();
    let mut events = Vec::new();
    let result = run(&mut state, &cfg, &DenyAllConfirmer, |e| events.push(e)).unwrap();
    let elapsed = started.elapsed();
    let envelope = envelope_for(&events, "hanger");
    assert_eq!(envelope["error"]["code"], "execution_error");
    assert!(envelope["error"]["message"]
        .as_str()
        .unwrap()
        .contains("tool was cancelled by user"));
    assert_eq!(result.finish_reason, "cancelled");
    // Interrupted by the stop request, not by the 30 s tool timeout.
    assert!(elapsed < Duration::from_secs(2), "stop took {elapsed:?}");
}

// --- tool error hints ---

#[test]
fn unknown_tool_error_carries_an_actionable_hint() {
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
    let message = envelope_for(&events, "nope")["error"]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(message.contains("unknown tool: nope"));
    assert!(message.contains("Hint: call list_available_tools"), "{message}");
}

// --- background entity links (links slice) ---

struct FakeProjectFileTool {
    fail: bool,
}

impl Tool for FakeProjectFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "fake project read_file for link tests (test-only tool)"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "rel_path": { "type": "string" } },
            "required": ["rel_path"],
            "additionalProperties": false
        })
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<ToolResult> {
        if self.fail {
            return Ok(ToolResult::err(ToolErrorCode::NotFound, "file not found"));
        }
        Ok(ToolResult::ok(
            json!({ "relPath": args["rel_path"], "content": "x" }),
        ))
    }
}

fn linked_targets(state: &Arc<crate::state::AppState>) -> Vec<(String, String)> {
    let guard = state.db.lock().unwrap();
    let conn = guard.as_ref().expect("test db installed");
    let mut stmt = conn
        .prepare("SELECT to_kind, to_id FROM links WHERE from_kind = 'chat' AND from_id = '7'")
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    rows
}

fn link_run(tool_fails: bool) -> Arc<crate::state::AppState> {
    crate::db::ensure_vec_extension();
    let state = Arc::new(crate::state::AppState::new());
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::db::schema::run_migrations(&conn).unwrap();
    *state.db.lock().unwrap() = Some(conn);
    let backend = ScriptedBackend::new(vec![
        tools_outcome(vec![call("c1", "read_file", json!({ "rel_path": "a.md" }))]),
        tools_outcome(vec![finish_call("done")]),
    ]);
    let mut catalog = Catalog::new();
    catalog.register(Arc::new(FinishTaskTool), true).unwrap();
    catalog
        .register(Arc::new(FakeProjectFileTool { fail: tool_fails }), true)
        .unwrap();
    let mut agent_state = AgentState {
        backend: &backend,
        catalog: &mut catalog,
    };
    let mut cfg = config(8);
    cfg.chat_id = Some(7);
    cfg.project_id = Some(3);
    cfg.state = Some(Arc::clone(&state));
    let result = run(&mut agent_state, &cfg, &DenyAllConfirmer, |_| {}).unwrap();
    assert_eq!(result.answer, "done");
    state
}

#[test]
fn successful_tool_call_links_chat_to_file_and_project() {
    let state = link_run(false);
    let pairs = linked_targets(&state);
    assert!(pairs.contains(&("project".to_string(), "3".to_string())), "{pairs:?}");
    assert!(pairs.contains(&("file".to_string(), "3:a.md".to_string())), "{pairs:?}");
}

#[test]
fn failed_tool_call_records_no_links() {
    let state = link_run(true);
    assert!(linked_targets(&state).is_empty());
}
