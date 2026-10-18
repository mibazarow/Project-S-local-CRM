use crate::{memory};
use crate::error::{Result};
use crate::state::{AppState};
use super::{with_db, emit_memory_changed};
use tauri::{AppHandle, State};

// Global memory CRUD for the UI (SPEC 6.3/6.5); mutations notify listeners
// with "memory-changed" so the memory list and open chats can refresh.
#[tauri::command]
pub(crate) fn memory_list(state: State<AppState>) -> Result<Vec<memory::MemoryDto>> {
    with_db(&state, memory::list)
}

#[tauri::command]
pub(crate) fn memory_add(
    app: AppHandle,
    state: State<AppState>,
    content: String,
    project_id: Option<i64>,
    tags: Option<Vec<String>>,
) -> Result<memory::MemoryDto> {
    let dto = with_db(&state, |conn| memory::add(conn, &content, project_id, tags, Some("user")))?;
    emit_memory_changed(&app, Some(dto.id));
    Ok(dto)
}

#[tauri::command]
pub(crate) fn memory_update(
    app: AppHandle,
    state: State<AppState>,
    id: i64,
    content: String,
    tags: Option<Vec<String>>,
) -> Result<memory::MemoryDto> {
    let dto = with_db(&state, |conn| memory::update(conn, id, &content, tags))?;
    emit_memory_changed(&app, Some(dto.id));
    Ok(dto)
}

#[tauri::command]
pub(crate) fn memory_delete(app: AppHandle, state: State<AppState>, id: i64) -> Result<()> {
    with_db(&state, |conn| memory::delete(conn, id))?;
    emit_memory_changed(&app, None);
    Ok(())
}

#[tauri::command]
pub(crate) fn memory_search(
    state: State<AppState>,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<memory::MemoryHit>> {
    let limit = limit.unwrap_or(10).clamp(1, 100);
    with_db(&state, |conn| memory::search(conn, &query, limit))
}

// Backfill the memory FTS mirror and vectors for records whose index rows
// are missing (legacy rows or vectors skipped while the model was absent).
#[tauri::command]
pub(crate) fn index_memory(state: State<AppState>) -> Result<usize> {
    memory::index_memory(&state)
}
