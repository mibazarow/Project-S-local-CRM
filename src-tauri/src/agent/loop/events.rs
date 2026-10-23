// Event and run-limit data contracts for the agent loop (SPEC 6.4). The event
// order and the camelCase wire shape are part of the UI/IPC contract.
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

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
