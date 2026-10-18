use crate::{links};
use crate::error::{Result};
use crate::state::{AppState};
use super::{with_db};
use tauri::{State};

// Entity links (links slice): the frontend records context-action links
// (note ↔ file/chat, task ↔ chat) after creating an entity from a file or a
// message; the agent loop records tool-call links on its own. Deduped by the
// UNIQUE constraint, so a repeated call is a no-op.
#[tauri::command]
pub(crate) fn link_create(
    state: State<AppState>,
    from_kind: String,
    from_id: String,
    to_kind: String,
    to_id: String,
) -> Result<()> {
    with_db(&state, |conn| {
        links::create(conn, &from_kind, &from_id, &to_kind, &to_id)
    })
}
