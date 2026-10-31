// The agent loop driver (SPEC 6.4): the run ends ONLY on finish_task or the
// step limit; the final assistant text of each round is latched as a fallback
// answer; the transcript is append-only for prompt-caching friendliness.
// Harness reliability (android-harness ideas, reimplemented in Rust): the
// false-success / loop / no-attempt guards, the run watchdog and the
// per-tool execution timeout; guards only append ephemeral working nudges to
// the model-facing transcript, never through on_message.
//
// This file is the public surface and the orchestration. The round body and
// guards live in `step`, tool-call execution in `tool_execution`, the watchdog
// in `cancel`, and the event/limit data contracts in `events`.
use super::catalog::Catalog;
use crate::confirm::Confirmer;
use crate::error::Result;
use crate::llm::{ChatEvent, ChatMessage, ChatOutcome, ChatParams, LlmClient, ToolSpec};
use serde::Serialize;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

#[path = "loop/cancel.rs"]
mod cancel;
#[path = "loop/events.rs"]
mod events;
#[path = "loop/step.rs"]
mod step;
#[cfg(test)]
#[path = "loop/tests.rs"]
mod tests;
#[path = "loop/tool_execution.rs"]
mod tool_execution;

pub use events::{AgentEvent, RunLimits};
use step::{Hooks, HookSet, Run, StepOutcome};

// Lets the loop run against a fake LLM in tests (no network).
pub trait ChatBackend: Send + Sync {
    fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolSpec]>,
        params: &ChatParams,
        on_event: &mut dyn FnMut(ChatEvent),
    ) -> Result<ChatOutcome>;

    fn chat_stream_cancellable(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolSpec]>,
        params: &ChatParams,
        on_event: &mut dyn FnMut(ChatEvent),
        cancel: Option<&AtomicBool>,
    ) -> Result<ChatOutcome> {
        let _ = cancel;
        self.chat_stream(messages, tools, params, on_event)
    }
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

    fn chat_stream_cancellable(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolSpec]>,
        params: &ChatParams,
        on_event: &mut dyn FnMut(ChatEvent),
        cancel: Option<&AtomicBool>,
    ) -> Result<ChatOutcome> {
        LlmClient::chat_stream_with_cancel(self, messages, tools, params, on_event, cancel)
    }
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
    // Chat-bound folder (see ToolCtx::scope); None uses `working_dir`.
    pub scope: Option<std::path::PathBuf>,
    // Canonical Notes/ vault root for notes-agent runs (notes tools only).
    pub notes_root: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRunResult {
    pub answer: String,
    // "finish" | "stop" | "step_limit" | "cancelled" | "timeout" | "stalled"
    // | "llm_error" (model request failed after a tool call already landed)
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
        let outcome = match run.call_llm(state, config, &mut hooks) {
            Ok(outcome) => outcome,
            Err(e) if run.is_cancelled(config) => {
                let _ = e;
                return Ok(run.finish_run(&mut hooks, "cancelled", step - 1, None));
            }
            // A tool call already landed in this run: a follow-up model
            // failure must not read as an operation failure. End with the
            // tool result (already persisted through on_message) plus a
            // visible non-fatal warning instead of an Error event.
            Err(e) if run.guards.tool_calls_made > 0 => {
                return Ok(run.finish_after_llm_error(&mut hooks, &e, step - 1));
            }
            Err(e) => {
                hooks.emit(AgentEvent::Error {
                    code: e.code().to_string(),
                    message: e.to_string(),
                });
                return Err(e);
            }
        };
        run.note_round_outcome(&outcome);
        if run.is_cancelled(config) {
            return Ok(run.finish_run(&mut hooks, "cancelled", step - 1, None));
        }
        match run.step(state, config, confirmer, &mut hooks, step, &outcome) {
            StepOutcome::Continue => continue,
            StepOutcome::Finished(result) => return Ok(result),
        }
    }

    let notice = format!("Остановлен по лимиту шагов: {}", config.max_steps);
    Ok(run.finish_run(&mut hooks, "step_limit", config.max_steps, Some(notice)))
}
