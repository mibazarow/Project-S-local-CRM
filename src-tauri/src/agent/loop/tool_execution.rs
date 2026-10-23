// Tool-call execution for the agent loop (SPEC 5.2 / 6.4): argument coercion
// and validation, lazy-discovery meta-tools, the two-level permission check and
// confirmation, the detached-worker per-tool timeout, and the actionable error
// hint + result-length cap applied on every path.
use super::events::AgentEvent;
use super::AgentRunConfig;
use crate::agent::catalog::{Catalog, EnableOutcome, Resolution, ENABLE_TOOLS, LIST_TOOLS};
use crate::agent::hints::hint_for;
use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::{coerce_args, validate_args, PermissionPolicy, Tool, ToolCtx};
use crate::confirm::{ConfirmDecision, ConfirmRequest, Confirmer};
use crate::llm::ToolCall;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

// Attaches an actionable hint to failed tool results (before capping, so the
// hint itself never blows the result budget) and enforces the tool's
// max_result_len on every error path, not just the executed-tool one.
pub(super) fn execute_tool_call(
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

    execute_with_timeout(tool, args, ctx, config.limits.tool_timeout, config.cancel.as_deref())
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
    cancel: Option<&AtomicBool>,
) -> ToolResult {
    let ctx = ctx.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = tool
            .execute(&args, &ctx)
            .unwrap_or_else(|e| ToolResult::err(ToolErrorCode::ExecutionError, e.to_string()));
        let _ = tx.send(result);
    });
    let deadline = Instant::now() + timeout;
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    "tool worker died before producing a result",
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return ToolResult::err(ToolErrorCode::ExecutionError, "tool was cancelled by user");
        }
        if Instant::now() >= deadline {
            return ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!("tool timed out after {}s", timeout.as_secs_f64()),
            );
        }
    }
}
