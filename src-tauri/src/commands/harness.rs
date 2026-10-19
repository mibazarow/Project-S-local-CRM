use crate::{attachments, chat, harness};
use crate::error::{Result};
use crate::state::{AgentRegistry, AppState, RunKey};
use super::{with_db, session_ttl_days};
use std::sync::Arc;
use tauri::{AppHandle, State};

// Harness sessions (SPEC 5.1): independent chats sharing only global memory.
#[tauri::command]
pub(crate) fn harness_create_session(
    state: State<AppState>,
    title: Option<String>,
    greeting: Option<String>,
) -> Result<harness::SessionDto> {
    with_db(&state, |conn| {
        harness::create_session_with_greeting(conn, title.as_deref(), greeting.as_deref())
    })
}

#[tauri::command]
pub(crate) fn harness_list_sessions(state: State<AppState>) -> Result<Vec<harness::SessionDto>> {
    with_db(&state, harness::list_sessions)
}

#[tauri::command]
pub(crate) fn harness_rename_session(
    state: State<AppState>,
    id: i64,
    title: String,
) -> Result<harness::SessionDto> {
    with_db(&state, |conn| harness::rename_session(conn, id, &title))
}

#[tauri::command]
pub(crate) fn harness_delete_session(state: State<AppState>, id: i64) -> Result<()> {
    with_db(&state, |conn| harness::delete_session(conn, id))
}

// Pin or unpin a Harness chat; returns the fresh session list so the sidebar
// reorders (pinned first) without a second read.
#[tauri::command]
pub(crate) fn harness_pin_session(
    state: State<AppState>,
    id: i64,
    pinned: bool,
) -> Result<Vec<harness::SessionDto>> {
    with_db(&state, |conn| harness::set_session_pinned(conn, id, pinned))
}

#[tauri::command]
pub(crate) fn harness_session_history(
    state: State<AppState>,
    id: i64,
) -> Result<Vec<chat::MessageDto>> {
    with_db(&state, |conn| harness::session_history(conn, id))
}

// Persists the user turn into the session's chat (with links to attachment
// copies already saved by attach_files) and starts the shared agent loop on
// a background thread; progress arrives as "agent-event" /
// "agent-confirmation" events carrying sessionId. Returns the session chat id.
#[tauri::command]
pub(crate) fn send_session_message(
    app: AppHandle,
    state: State<AppState>,
    registry: State<'_, Arc<AgentRegistry>>,
    session_id: i64,
    text: String,
    attachments: Option<Vec<attachments::AttachmentRef>>,
) -> Result<i64> {
    harness::run_session(
        &app,
        &state,
        registry.inner(),
        session_id,
        &text,
        "harness",
        attachments.as_deref().unwrap_or(&[]),
    )
}

#[tauri::command]
pub(crate) fn cancel_session_run(registry: State<'_, Arc<AgentRegistry>>, session_id: i64) -> Result<bool> {
    Ok(registry.request_cancel(RunKey::Session(session_id)))
}

// Deletes harness sessions idle past app.json `sessionTtlDays` (0 disables).
#[tauri::command]
pub(crate) fn purge_sessions(state: State<AppState>) -> Result<usize> {
    let wd = state.working_dir()?;
    let ttl = session_ttl_days(&wd);
    with_db(&state, |conn| harness::purge_inactive_sessions(conn, ttl))
}
