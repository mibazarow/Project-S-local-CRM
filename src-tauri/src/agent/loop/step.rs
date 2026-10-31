// The per-run step machinery (SPEC 6.4): the mutable run state threaded across
// rounds, the once-per-run reliability guards (false-success / loop / no-attempt),
// the finish_task latch, and the run/step callbacks. The orchestration that
// drives these steps lives in the parent `loop` module.
use super::cancel::Watchdog;
use super::events::AgentEvent;
use super::tool_execution::execute_tool_call;
use super::{AgentRunConfig, AgentRunResult, AgentState};
use crate::agent::catalog::{Catalog, ENABLE_TOOLS, LIST_TOOLS};
use crate::agent::result::ToolResult;
use crate::agent::tool::ToolCtx;
use crate::confirm::Confirmer;
use crate::error::Result;
use crate::llm::{sanitize_tool_history, ChatEvent, ChatMessage, ChatOutcome, ToolCall};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::sync::atomic::Ordering;
use std::time::Duration;

// Guard nudges are model-facing English heuristics; each guard fires at most
// once per run and only appends an ephemeral working message to the
// model-facing transcript (never persisted through on_message).
pub(super) const FALSE_SUCCESS_NUDGE: &str = "You claim completion without verification. Verify with a tool or call finish_task with the result.";
pub(super) const LOOP_NUDGE: &str = "You are repeating the same call with the same result. Change approach or call finish_task.";
pub(super) const NO_ATTEMPT_NUDGE: &str = "You have tools. Attempt the task before giving up.";
// Consecutive identical call+result signatures before the loop nudge fires.
pub(super) const LOOP_REPEAT_LIMIT: usize = 3;

// User-facing warning for a run whose action already landed but whose
// follow-up model request failed: the operation is not presented as failed.
pub(super) const LLM_AFTER_TOOL_WARNING: &str = "Действие выполнено, но ответ модели не получен.";

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

// The two run callbacks threaded through every step: `emit` for UI events,
// `on_message` for transcript persistence.
pub(super) struct Hooks<'a, E, M>
where
    E: FnMut(AgentEvent),
    M: FnMut(&ChatMessage),
{
    pub(super) emit: &'a mut E,
    pub(super) on_message: &'a mut M,
}

// Lets the steps take any reborrow of the hooks struct, not a concrete one.
pub(super) trait HookSet {
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
pub(super) enum StepOutcome {
    Continue,
    Finished(AgentRunResult),
}

// Per-run mutable state shared by the steps: the append-only transcript, the
// latched fallback answer, the accumulated thinking trace, the confirmation
// allowlist, the watchdog and the once-per-run guard counters.
pub(super) struct Run {
    pub(super) messages: Vec<ChatMessage>,
    ctx: ToolCtx,
    pub(super) fallback: String,
    // Thinking trace accumulated across rounds: replayed to the model on every
    // assistant turn (thinking-mode providers reject a follow-up without it)
    // and reported with the final result.
    pub(super) reasoning: Option<String>,
    always_allow: BTreeSet<String>,
    watchdog: Watchdog,
    // This round's thinking trace, set by note_round_outcome and consumed by
    // step when it appends the assistant message.
    round_reasoning: Option<String>,
    // Guard state, all per-run: each guard fires at most once.
    pub(super) guards: GuardState,
}

// Guard bookkeeping shared by the FalseSuccess / NoAttempt / Loop guards.
pub(super) struct GuardState {
    pub(super) tool_calls_made: usize,
    pub(super) verifying_ok: bool,
    last_call_sig: Option<u64>,
    repeat_count: usize,
    false_success_nudged: bool,
    pub(super) loop_nudged: bool,
    no_attempt_nudged: bool,
}

impl Run {
    pub(super) fn new(config: &AgentRunConfig, clock: Option<Box<dyn Fn() -> Duration + Send>>) -> Self {
        // The pre-built transcript is sanitized as a last-line guard: a caller
        // that assembled its history outside chat::load_history must still never
        // send an assistant tool_calls turn without its tool results (HTTP 400).
        let messages = sanitize_tool_history(&config.messages);
        let ctx = ToolCtx {
            project_id: config.project_id,
            session_id: config.session_id,
            chat_id: config.chat_id,
            working_dir: config.working_dir.clone(),
            scope: config.scope.clone(),
            state: config.state.clone(),
            notes_root: config.notes_root.clone(),
            cancel: config.cancel.clone(),
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

    pub(super) fn is_cancelled(&self, config: &AgentRunConfig) -> bool {
        config
            .cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    pub(super) fn check_watchdog(&self) -> Option<&'static str> {
        self.watchdog.check()
    }

    // One streamed LLM round; deltas count as watchdog progress. A backend
    // error is returned to the caller, which decides how it surfaces: fatal
    // Error event, or the after-tool warning when an action already landed.
    pub(super) fn call_llm<H: HookSet>(
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
        state.backend.chat_stream_cancellable(
            &self.messages,
            Some(&specs),
            &config.params,
            &mut on_event,
            config.cancel.as_deref(),
        )
    }

    // Accumulate this round's thinking trace and latch its final text as the
    // fallback answer candidate, before any transcript turns are appended.
    // The round's trace is kept for the assistant message appended by `step`.
    pub(super) fn note_round_outcome(&mut self, outcome: &ChatOutcome) {
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
    pub(super) fn step<H: HookSet>(
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
            if self.is_cancelled(config) {
                return StepOutcome::Finished(self.finish_run(hooks, "cancelled", step, None));
            }
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
    pub(super) fn note_tool_call(&mut self, tc: &ToolCall, envelope_json: &Value, ok: bool) {
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
    pub(super) fn handle_loop_guard(&mut self) {
        if !self.guards.loop_nudged && self.guards.repeat_count >= LOOP_REPEAT_LIMIT {
            self.messages.push(ChatMessage::system(LOOP_NUDGE));
            self.guards.loop_nudged = true;
        }
    }

    // The finish_task latch: a successful finish_task call ends the run with
    // its answer/action arguments.
    pub(super) fn finish_latch(&self, tc: &ToolCall, ok: bool) -> Option<(String, String)> {
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

    // A backend failure once at least one tool call has already landed: the
    // run ends as a partial success, not an error. The tool result is already
    // in the transcript (persisted through on_message); the final answer is
    // the user-facing warning plus the transport error for diagnosis, so the
    // completed action is never presented as a failure.
    pub(super) fn finish_after_llm_error<H: HookSet>(
        &mut self,
        hooks: &mut H,
        err: &crate::error::AppError,
        steps: usize,
    ) -> AgentRunResult {
        let answer = format!("{LLM_AFTER_TOOL_WARNING}\n{err}");
        self.finish_run(hooks, "llm_error", steps, Some(answer))
    }

    // The single exit for every non-error end: emit Done and build the result
    // with the steps completed. The finish latch overrides the answer with the
    // one from finish_task's arguments; every other end reports the latch.
    pub(super) fn finish_run<H: HookSet>(
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
