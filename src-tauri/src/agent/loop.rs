// The agent loop driver (SPEC 6.4): the run ends ONLY on finish_task or the
// step limit; the final assistant text of each round is latched as a fallback
// answer; the transcript is append-only for prompt-caching friendliness.
// Harness reliability (android-harness ideas, reimplemented in Rust): the
// false-success / loop / no-attempt guards, the run watchdog and the
// per-tool execution timeout below; guards only append ephemeral working
// nudges to the model-facing transcript, never through on_message.
use super::catalog::{Catalog, EnableOutcome, Resolution, ENABLE_TOOLS, LIST_TOOLS};
use super::hints::hint_for;
use crate::confirm::{ConfirmDecision, ConfirmRequest, Confirmer};
use super::result::{ToolErrorCode, ToolResult};
use super::tool::{coerce_args, validate_args, PermissionPolicy, Tool, ToolCtx};
use crate::error::Result;
use crate::llm::{
    sanitize_tool_history, ChatEvent, ChatMessage, ChatOutcome, ChatParams, LlmClient, ToolCall,
    ToolSpec,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    // Thinking-mode trace of the running round (streamed live to the UI).
    ReasoningDelta {
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
        // Thinking trace accumulated over the whole run (None when the model
        // never produced one or thinking is off).
        reasoning: Option<String>,
    },
    Error {
        code: String,
        message: String,
    },
}

// Run-level watchdog limits: checked at step boundaries; progress is any LLM
// text delta or completed tool call. A backend call that itself hangs is
// bounded by the LLM client's socket timeouts, not by this watchdog.
#[derive(Debug, Clone)]
pub struct RunLimits {
    // Hard wall-clock cap for the whole run.
    pub max_duration: Duration,
    // Max gap between two progress events before the run counts as hung.
    pub stall_timeout: Duration,
    // Per-tool execution timeout; the detached worker may still finish later.
    pub tool_timeout: Duration,
}

impl Default for RunLimits {
    fn default() -> Self {
        RunLimits {
            max_duration: Duration::from_secs(30 * 60),
            stall_timeout: Duration::from_secs(10 * 60),
            tool_timeout: Duration::from_secs(240),
        }
    }
}

struct Watchdog {
    limits: RunLimits,
    // Elapsed time since run start; injectable so tests can fake the clock.
    now: Box<dyn Fn() -> Duration>,
    last_progress: Duration,
}

impl Watchdog {
    fn system(limits: &RunLimits) -> Self {
        let started = Instant::now();
        Self::new(limits, Box::new(move || started.elapsed()))
    }

    fn new(limits: &RunLimits, now: Box<dyn Fn() -> Duration>) -> Self {
        Watchdog {
            limits: limits.clone(),
            now,
            last_progress: Duration::ZERO,
        }
    }

    fn note_progress(&mut self) {
        self.last_progress = (self.now)();
    }

    fn check(&self) -> Option<&'static str> {
        let elapsed = (self.now)();
        if elapsed >= self.limits.max_duration {
            Some("timeout")
        } else if elapsed.saturating_sub(self.last_progress) >= self.limits.stall_timeout {
            Some("stalled")
        } else {
            None
        }
    }
}

// Guard nudges are model-facing English heuristics; each guard fires at most
// once per run and only appends an ephemeral working message to the
// model-facing transcript (never persisted through on_message).
const FALSE_SUCCESS_NUDGE: &str = "You claim completion without verification. Verify with a tool or call finish_task with the result.";
const LOOP_NUDGE: &str = "You are repeating the same call with the same result. Change approach or call finish_task.";
const NO_ATTEMPT_NUDGE: &str = "You have tools. Attempt the task before giving up.";
// Consecutive identical call+result signatures before the loop nudge fires.
const LOOP_REPEAT_LIMIT: usize = 3;

const COMPLETION_PHRASES: [&str; 5] = ["готово", "сделано", "выполнено", "done", "completed"];
const INCAPABILITY_PHRASES: [&str; 5] = ["не могу", "я не могу", "i can't", "i cannot", "unable to"];

fn claims_completion(text: &str) -> bool {
    let lower = text.to_lowercase();
    COMPLETION_PHRASES.iter().any(|p| lower.contains(p))
}

fn claims_incapability(text: &str) -> bool {
    let lower = text.to_lowercase();
    INCAPABILITY_PHRASES.iter().any(|p| lower.contains(p))
}

// LoopGuard signature: tool name + canonical args + serialized result.
// serde_json maps are BTreeMap-backed (no preserve_order feature), so
// to_string is key-sorted and identical calls hash identically.
fn call_signature(tc: &ToolCall, envelope_json: &Value) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    tc.name.hash(&mut hasher);
    tc.arguments.to_string().hash(&mut hasher);
    envelope_json.to_string().hash(&mut hasher);
    hasher.finish()
}

pub struct AgentRunConfig {
    // Pre-built model-facing transcript: system prompt + history (possibly
    // folded into checkpoint summaries by chat::context) + the fresh user
    // turn. The loop only appends to it, never rewrites the prefix.
    pub messages: Vec<ChatMessage>,
    pub max_steps: usize,
    // Watchdog caps: hard run duration, progress stall timeout, per-tool timeout.
    pub limits: RunLimits,
    // Full autopilot (SPEC 5.2): every confirmation is auto-allowed.
    pub allow_all: bool,
    pub project_id: Option<i64>,
    // Harness session of this run, when it is one (schedule tools bind to it).
    pub session_id: Option<i64>,
    // Chat the run writes into; handed to tools through ToolCtx so send_file
    // can append its attachment to the right chat (SPEC 6.1).
    pub chat_id: Option<i64>,
    pub params: ChatParams,
    // Checked between steps; set by cancel_run for this project's run.
    pub cancel: Option<Arc<AtomicBool>>,
    // Handed to every tool through ToolCtx: fs/task/memory tools borrow the
    // run thread's DB connection and working dir through it.
    pub state: Option<Arc<crate::state::AppState>>,
    pub working_dir: Option<std::path::PathBuf>,
    // Canonical Notes/ vault root for notes-agent runs (notes tools only).
    pub notes_root: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRunResult {
    pub answer: String,
    // "finish" | "stop" | "step_limit" | "cancelled" | "timeout" | "stalled"
    pub finish_reason: String,
    pub steps: usize,
    // Thinking trace of the run, accumulated across rounds.
    pub reasoning: Option<String>,
}

pub struct AgentState<'a> {
    pub backend: &'a dyn ChatBackend,
    pub catalog: &'a mut Catalog,
}

#[cfg(test)]
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
    emit: impl FnMut(AgentEvent),
    on_message: impl FnMut(&ChatMessage),
) -> Result<AgentRunResult> {
    run_inner(state, config, confirmer, emit, on_message, None)
}

// clock overrides the watchdog's elapsed-time source (tests fake it); None
// means the real monotonic clock. The body is decomposed into the named steps
// below; the round/event order, guard firing, step-limit/latch semantics and
// error handling are exactly what a single flat loop body used to do.
fn run_inner(
    state: &mut AgentState<'_>,
    config: &AgentRunConfig,
    confirmer: &dyn Confirmer,
    mut emit: impl FnMut(AgentEvent),
    mut on_message: impl FnMut(&ChatMessage),
    clock: Option<Box<dyn Fn() -> Duration + Send>>,
) -> Result<AgentRunResult> {
    let mut run = Run::new(config, clock);
    let mut hooks = Hooks {
        emit: &mut emit,
        on_message: &mut on_message,
    };

    for step in 1..=config.max_steps {
        if run.is_cancelled(config) {
            return Ok(run.finish_run(&mut hooks, "cancelled", step - 1, None));
        }
        if let Some(reason) = run.check_watchdog() {
            return Ok(run.finish_run(&mut hooks, reason, step - 1, None));
        }
        let outcome = run.call_llm(state, config, &mut hooks)?;
        run.note_round_outcome(&outcome);
        match run.step(state, config, confirmer, &mut hooks, step, &outcome) {
            StepOutcome::Continue => continue,
            StepOutcome::Finished(result) => return Ok(result),
        }
    }

    Ok(run.finish_run(&mut hooks, "step_limit", config.max_steps, None))
}

// The two run callbacks threaded through every step: `emit` for UI events,
// `on_message` for transcript persistence.
struct Hooks<'a, E, M>
where
    E: FnMut(AgentEvent),
    M: FnMut(&ChatMessage),
{
    emit: &'a mut E,
    on_message: &'a mut M,
}

// Lets the steps take any reborrow of the hooks struct, not a concrete one.
trait HookSet {
    fn emit(&mut self, event: AgentEvent);
    fn on_message(&mut self, message: &ChatMessage);

    // The pre-existing execute_tool_call boundary takes a bare emit closure;
    // this adapter routes its ConfirmationRequested/Started events through the
    // same hook without changing that function.
    fn exec_tool(
        &mut self,
        catalog: &mut Catalog,
        tc: &ToolCall,
        config: &AgentRunConfig,
        confirmer: &dyn Confirmer,
        always_allow: &mut BTreeSet<String>,
        ctx: &ToolCtx,
    ) -> ToolResult {
        execute_tool_call(catalog, tc, config, confirmer, always_allow, ctx, &mut |e| {
            self.emit(e)
        })
    }
}

impl<E, M> HookSet for Hooks<'_, E, M>
where
    E: FnMut(AgentEvent),
    M: FnMut(&ChatMessage),
{
    fn emit(&mut self, event: AgentEvent) {
        (self.emit)(event);
    }

    fn on_message(&mut self, message: &ChatMessage) {
        (self.on_message)(message);
    }
}

// What one round decided after its tool calls: continue with the next round
// or end the run (the finish_task latch fired).
enum StepOutcome {
    Continue,
    Finished(AgentRunResult),
}

// Per-run mutable state shared by the steps: the append-only transcript, the
// latched fallback answer, the accumulated thinking trace, the confirmation
// allowlist, the watchdog and the once-per-run guard counters.
struct Run {
    messages: Vec<ChatMessage>,
    ctx: ToolCtx,
    fallback: String,
    // Thinking trace accumulated across rounds: replayed to the model on every
    // assistant turn (thinking-mode providers reject a follow-up without it)
    // and reported with the final result.
    reasoning: Option<String>,
    always_allow: BTreeSet<String>,
    watchdog: Watchdog,
    // This round's thinking trace, set by note_round_outcome and consumed by
    // step when it appends the assistant message.
    round_reasoning: Option<String>,
    // Guard state, all per-run: each guard fires at most once.
    guards: GuardState,
}

// Guard bookkeeping shared by the FalseSuccess / NoAttempt / Loop guards.
struct GuardState {
    tool_calls_made: usize,
    verifying_ok: bool,
    last_call_sig: Option<u64>,
    repeat_count: usize,
    false_success_nudged: bool,
    loop_nudged: bool,
    no_attempt_nudged: bool,
}

impl Run {
    fn new(config: &AgentRunConfig, clock: Option<Box<dyn Fn() -> Duration + Send>>) -> Self {
        // The pre-built transcript is sanitized as a last-line guard: a caller
        // that assembled its history outside chat::load_history must still never
        // send an assistant tool_calls turn without its tool results (HTTP 400).
        let messages = sanitize_tool_history(&config.messages);
        let ctx = ToolCtx {
            project_id: config.project_id,
            session_id: config.session_id,
            chat_id: config.chat_id,
            working_dir: config.working_dir.clone(),
            state: config.state.clone(),
            notes_root: config.notes_root.clone(),
        };
        let watchdog = match clock {
            Some(now) => Watchdog::new(&config.limits, now),
            None => Watchdog::system(&config.limits),
        };
        Run {
            messages,
            ctx,
            fallback: String::new(),
            reasoning: None,
            always_allow: BTreeSet::new(),
            watchdog,
            round_reasoning: None,
            guards: GuardState {
                tool_calls_made: 0,
                verifying_ok: false,
                last_call_sig: None,
                repeat_count: 0,
                false_success_nudged: false,
                loop_nudged: false,
                no_attempt_nudged: false,
            },
        }
    }

    fn is_cancelled(&self, config: &AgentRunConfig) -> bool {
        config
            .cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    fn check_watchdog(&self) -> Option<&'static str> {
        self.watchdog.check()
    }

    // One streamed LLM round; deltas count as watchdog progress. A backend
    // error emits the llm Error event and aborts the run (same as before).
    fn call_llm<H: HookSet>(
        &mut self,
        state: &mut AgentState<'_>,
        config: &AgentRunConfig,
        hooks: &mut H,
    ) -> Result<ChatOutcome> {
        let specs = state.catalog.to_specs();
        let mut on_event = |ev: ChatEvent| match ev {
            ChatEvent::Delta(text) => {
                self.watchdog.note_progress();
                hooks.emit(AgentEvent::TextDelta { text });
            }
            ChatEvent::ReasoningDelta(text) => {
                self.watchdog.note_progress();
                hooks.emit(AgentEvent::ReasoningDelta { text });
            }
            _ => {}
        };
        match state.backend.chat_stream(&self.messages, Some(&specs), &config.params, &mut on_event) {
            Ok(outcome) => Ok(outcome),
            Err(e) => {
                hooks.emit(AgentEvent::Error {
                    code: "llm".into(),
                    message: e.to_string(),
                });
                Err(e)
            }
        }
    }

    // Accumulate this round's thinking trace and latch its final text as the
    // fallback answer candidate, before any transcript turns are appended.
    // The round's trace is kept for the assistant message appended by `step`.
    fn note_round_outcome(&mut self, outcome: &ChatOutcome) {
        let round_reasoning = outcome
            .reasoning
            .clone()
            .filter(|r| !r.trim().is_empty());
        if let Some(r) = &round_reasoning {
            self.reasoning = Some(match self.reasoning.take() {
                Some(prev) => format!("{prev}\n{r}"),
                None => r.clone(),
            });
        }
        if let Some(content) = outcome.content.as_deref().filter(|c| !c.trim().is_empty()) {
            self.fallback = content.to_string();
        }
        self.round_reasoning = round_reasoning;
    }

    // The round body after the trace/fallback bookkeeping: text-only rounds
    // append their assistant turn, fire the claim guards and continue; tool
    // rounds append the assistant tool_calls turn and run every call.
    fn step<H: HookSet>(
        &mut self,
        state: &mut AgentState<'_>,
        config: &AgentRunConfig,
        confirmer: &dyn Confirmer,
        hooks: &mut H,
        step: usize,
        outcome: &ChatOutcome,
    ) -> StepOutcome {
        let round_reasoning = self.round_reasoning.take();
        if outcome.tool_calls.is_empty() {
            if let Some(content) = &outcome.content {
                let msg = ChatMessage::assistant(content.clone()).with_reasoning(round_reasoning);
                hooks.on_message(&msg);
                self.messages.push(msg);
                self.handle_claim_guards(content);
            }
            return StepOutcome::Continue;
        }
        let assistant_msg = ChatMessage::assistant_tool_calls(outcome.tool_calls.clone())
            .with_reasoning(round_reasoning);
        hooks.on_message(&assistant_msg);
        self.messages.push(assistant_msg);

        for tc in &outcome.tool_calls {
            if let Some((answer, reason)) =
                self.run_tool_call(state.catalog, tc, config, confirmer, hooks)
            {
                return StepOutcome::Finished(self.finish_run(hooks, &reason, step, Some(answer)));
            }
        }
        StepOutcome::Continue
    }

    // FalseSuccessGuard: completion claim with no successful verifying tool
    // call in this run. NoAttemptGuard: incapability claim with zero tool
    // calls. Each fires at most once per run.
    fn handle_claim_guards(&mut self, content: &str) {
        if !self.guards.false_success_nudged
            && !self.guards.verifying_ok
            && claims_completion(content)
        {
            self.messages.push(ChatMessage::system(FALSE_SUCCESS_NUDGE));
            self.guards.false_success_nudged = true;
        }
        if !self.guards.no_attempt_nudged
            && self.guards.tool_calls_made == 0
            && claims_incapability(content)
        {
            self.messages.push(ChatMessage::system(NO_ATTEMPT_NUDGE));
            self.guards.no_attempt_nudged = true;
        }
    }

    // One tool call end to end: Started event, execution, envelope
    // serialization, Finished event, best-effort entity linking, the tool
    // transcript message, guard bookkeeping and the finish_task latch.
    // Returns Some((answer, reason)) when the latch fires.
    fn run_tool_call<H: HookSet>(
        &mut self,
        catalog: &mut Catalog,
        tc: &ToolCall,
        config: &AgentRunConfig,
        confirmer: &dyn Confirmer,
        hooks: &mut H,
    ) -> Option<(String, String)> {
        hooks.emit(AgentEvent::ToolCallStarted {
            name: tc.name.clone(),
            args: tc.arguments.clone(),
        });
        let envelope = hooks.exec_tool(
            catalog,
            tc,
            config,
            confirmer,
            &mut self.always_allow,
            &self.ctx,
        );
        let ok = envelope.ok;
        // The envelope serialization must never abort the run with `?`:
        // an early return here would persist the assistant tool_calls
        // turn without its tool result and poison every later request.
        let envelope_json = serde_json::to_value(&envelope).unwrap_or_else(|e| {
            json!({
                "ok": false,
                "error": {
                    "code": "execution_error",
                    "message": format!("tool result could not be serialized: {e}"),
                },
            })
        });
        hooks.emit(AgentEvent::ToolCallFinished {
            name: tc.name.clone(),
            ok,
            envelope: envelope_json.clone(),
        });
        // Background entity linking (links slice): a successful tool call
        // ties the run's chat to the file/task/note/schedule it touched.
        // Best-effort: the helper swallows every error and never changes
        // the tool result or the loop.
        if ok {
            crate::links::auto_link_from_tool_ctx(&self.ctx, &tc.name, &tc.arguments, &envelope_json);
        }
        // Every tool call — success, error, denied, timeout, hidden,
        // unknown, invalid args or a killed run — reaches this line and
        // appends its tool message before the next assistant turn.
        let tool_content = serde_json::to_string(&envelope_json).unwrap_or_else(|_| {
            r#"{"ok":false,"error":{"code":"execution_error","message":"tool result could not be serialized"}}"#
                .to_string()
        });
        let tool_msg = ChatMessage::tool_result(&tc.id, tool_content);
        hooks.on_message(&tool_msg);
        self.messages.push(tool_msg);
        self.watchdog.note_progress();

        self.note_tool_call(tc, &envelope_json, ok);
        // The finish latch is checked before the LoopGuard nudge: a run that
        // ends on finish_task never gets the nudge appended (as before).
        if let Some(latched) = self.finish_latch(tc, ok) {
            return Some(latched);
        }
        self.handle_loop_guard();
        None
    }

    // Guard bookkeeping after a tool result was appended: the call count and
    // the verifying evidence for FalseSuccess, the identical-call streak.
    fn note_tool_call(&mut self, tc: &ToolCall, envelope_json: &Value, ok: bool) {
        self.guards.tool_calls_made += 1;
        // Discovery meta-tools prove nothing about the task itself, so a
        // successful one is not verifying evidence for the false-success
        // guard.
        if ok && tc.name != LIST_TOOLS && tc.name != ENABLE_TOOLS {
            self.guards.verifying_ok = true;
        }
        let sig = call_signature(tc, envelope_json);
        if Some(sig) == self.guards.last_call_sig {
            self.guards.repeat_count += 1;
        } else {
            self.guards.last_call_sig = Some(sig);
            self.guards.repeat_count = 1;
        }
    }

    // LoopGuard: the same call produced the same result LOOP_REPEAT_LIMIT
    // times in a row; fires at most once per run.
    fn handle_loop_guard(&mut self) {
        if !self.guards.loop_nudged && self.guards.repeat_count >= LOOP_REPEAT_LIMIT {
            self.messages.push(ChatMessage::system(LOOP_NUDGE));
            self.guards.loop_nudged = true;
        }
    }

    // The finish_task latch: a successful finish_task call ends the run with
    // its answer/action arguments.
    fn finish_latch(&self, tc: &ToolCall, ok: bool) -> Option<(String, String)> {
        if tc.name != "finish_task" || !ok {
            return None;
        }
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
        Some((answer, reason))
    }

    // The single exit for every non-error end: emit Done and build the result
    // with the steps completed. The finish latch overrides the answer with the
    // one from finish_task's arguments; every other end reports the latch.
    fn finish_run<H: HookSet>(
        &mut self,
        hooks: &mut H,
        reason: &str,
        steps: usize,
        answer: Option<String>,
    ) -> AgentRunResult {
        let answer = answer.unwrap_or_else(|| self.fallback.clone());
        hooks.emit(AgentEvent::Done {
            answer: answer.clone(),
            reason: reason.into(),
            reasoning: self.reasoning.clone(),
        });
        AgentRunResult {
            answer,
            finish_reason: reason.into(),
            steps,
            reasoning: self.reasoning.clone(),
        }
    }
}

// Attaches an actionable hint to failed tool results (before capping, so the
// hint itself never blows the result budget) and enforces the tool's
// max_result_len on every error path, not just the executed-tool one.
fn execute_tool_call(
    catalog: &mut Catalog,
    tc: &ToolCall,
    config: &AgentRunConfig,
    confirmer: &dyn Confirmer,
    always_allow: &mut BTreeSet<String>,
    ctx: &ToolCtx,
    emit: &mut impl FnMut(AgentEvent),
) -> ToolResult {
    let result = execute_inner(catalog, tc, config, confirmer, always_allow, ctx, emit);
    let max_len = match catalog.resolve(&tc.name) {
        Resolution::Found(tool) => tool.max_result_len(),
        // Meta/unknown/hidden paths have no Tool object: the Tool trait default.
        _ => 8_000,
    };
    attach_hint(result, &tc.name).capped(max_len)
}

fn attach_hint(mut result: ToolResult, tool: &str) -> ToolResult {
    if let Some(err) = result.error.as_mut() {
        if let Some(code) = ToolErrorCode::from_code(&err.code) {
            if let Some(hint) = hint_for(tool, code, &err.message) {
                err.message.push_str(&format!("\nHint: {hint}"));
            }
        }
    }
    result
}

fn execute_inner(
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

    let args = coerce_args(&tool.parameters(), &tc.arguments);
    if let Err(msg) = validate_args(&tool.parameters(), &args) {
        return ToolResult::err(ToolErrorCode::InvalidArgs, msg);
    }

    let danger = tool.danger_reason(&args, ctx);
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
            args: args.clone(),
            danger_reason: danger.clone(),
        });
        let req = ConfirmRequest {
            tool: tc.name.clone(),
            args: args.clone(),
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

    execute_with_timeout(tool, args, ctx, config.limits.tool_timeout)
}

// The tool runs on a detached worker so a hung tool cannot wedge the run: on
// timeout the loop continues with an execution_error result, while the worker
// may still finish later and its side effects still land (documented
// limitation of the synchronous harness).
fn execute_with_timeout(
    tool: Arc<dyn Tool>,
    args: Value,
    ctx: &ToolCtx,
    timeout: Duration,
) -> ToolResult {
    let ctx = ctx.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = tool
            .execute(&args, &ctx)
            .unwrap_or_else(|e| ToolResult::err(ToolErrorCode::ExecutionError, e.to_string()));
        let _ = tx.send(result);
    });
    rx.recv_timeout(timeout).unwrap_or_else(|_| {
        ToolResult::err(
            ToolErrorCode::ExecutionError,
            format!("tool timed out after {}s", timeout.as_secs_f64()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::finish::FinishTaskTool;
    use crate::agent::tool::{EchoTool, PermissionPolicy};
    use crate::confirm::{AutoConfirmer, DenyAllConfirmer};
    use crate::error::AppError;
    use crate::llm::Role;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

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
}
