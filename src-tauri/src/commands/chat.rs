use crate::{attachments, chat};
use crate::agent::permissions::ConfirmDecision;
use crate::error::{AppError, Result};
use crate::state::{AgentRegistry, AppState, RunKey};
use super::{with_db};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

// Persists the user turn into the project's chat (with links to attachment
// copies already saved by attach_files) and starts the agent loop on a
// background thread; progress arrives as "agent-event" and
// "agent-confirmation" events. Returns the chat id.
#[tauri::command]
pub(crate) fn send_project_message(
    app: AppHandle,
    state: State<AppState>,
    registry: State<'_, Arc<AgentRegistry>>,
    project_id: i64,
    text: String,
    attachments: Option<Vec<attachments::AttachmentRef>>,
) -> Result<i64> {
    chat::start_project_run(
        &app,
        &state,
        registry.inner(),
        project_id,
        &text,
        attachments.as_deref().unwrap_or(&[]),
    )
}

#[tauri::command]
pub(crate) fn resolve_confirmation(
    registry: State<'_, Arc<AgentRegistry>>,
    id: u64,
    decision: String,
) -> Result<()> {
    let decision = match decision.as_str() {
        "allow" => ConfirmDecision::Allow,
        "deny" => ConfirmDecision::Deny,
        "allowAlways" => ConfirmDecision::AllowAlways,
        other => return Err(AppError::Invalid(format!("unknown decision: {other}"))),
    };
    registry.resolve(id, decision)
}

#[tauri::command]
pub(crate) fn project_chat_history(
    state: State<AppState>,
    project_id: i64,
) -> Result<Vec<chat::MessageDto>> {
    with_db(&state, |conn| chat::chat_history(conn, project_id))
}

// Sets the cancel flag of the project's active run; the loop stops between
// steps. Returns false when no run is active.
#[tauri::command]
pub(crate) fn cancel_run(registry: State<'_, Arc<AgentRegistry>>, project_id: i64) -> Result<bool> {
    Ok(registry.request_cancel(RunKey::Project(project_id)))
}

// Message-level chat UX (SPEC chat UX slice): pin, truncate (regenerate),
// branch (harness only), Markdown export and in-chat LIKE search.
#[tauri::command]
pub(crate) fn chat_pin(state: State<AppState>, chat_id: i64, message_id: i64, pinned: bool) -> Result<()> {
    with_db(&state, |conn| chat::set_pinned(conn, chat_id, message_id, pinned))
}

#[tauri::command]
pub(crate) fn chat_truncate_after(state: State<AppState>, chat_id: i64, message_id: i64) -> Result<usize> {
    with_db(&state, |conn| chat::truncate_after(conn, chat_id, message_id))
}

// Forks a harness session at a message; "sessions-changed" makes the fresh
// session appear in the sidebar right away (same as schedule_open).
#[tauri::command]
pub(crate) fn chat_branch(
    app: AppHandle,
    state: State<AppState>,
    chat_id: i64,
    message_id: i64,
) -> Result<chat::ChatBranchDto> {
    let dto = with_db(&state, |conn| chat::branch_chat(conn, chat_id, message_id))?;
    let _ = app.emit("sessions-changed", serde_json::json!({}));
    Ok(dto)
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChatExportResult {
    markdown: String,
}

#[tauri::command]
pub(crate) fn chat_export(state: State<AppState>, chat_id: i64) -> Result<ChatExportResult> {
    let markdown = with_db(&state, |conn| chat::export_markdown(conn, chat_id))?;
    Ok(ChatExportResult { markdown })
}

#[tauri::command]
pub(crate) fn chat_search(
    state: State<AppState>,
    chat_id: i64,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<chat::ChatSearchHitDto>> {
    let limit = limit.unwrap_or(50).clamp(1, 200);
    with_db(&state, |conn| chat::search_messages(conn, chat_id, &query, limit))
}
