use crate::{index, search};
use crate::error::{Result};
use crate::state::{AppState};
use tauri::{State};

#[tauri::command]
pub(crate) fn search_notes(
    state: State<AppState>,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<search::SearchHit>> {
    let limit = limit.unwrap_or(30).clamp(1, 100);
    search::search_notes(&state, &query, limit)
}

#[tauri::command]
pub(crate) fn semantic_search(
    state: State<AppState>,
    project_id: i64,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<index::SemanticHit>> {
    let limit = limit.unwrap_or(10).clamp(1, 100);
    index::semantic_search(&state, project_id, &query, limit)
}

#[tauri::command]
pub(crate) fn search_project(
    state: State<AppState>,
    project_id: i64,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<search::SearchHit>> {
    let limit = limit.unwrap_or(30).clamp(1, 100);
    search::search_project(&state, project_id, &query, limit)
}

#[tauri::command]
pub(crate) fn search_chats(
    state: State<AppState>,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<search::ChatHit>> {
    let limit = limit.unwrap_or(10).clamp(1, 50);
    search::search_chats(&state, &query, limit)
}

// Global search over all entities for the ⌘K palette.
#[tauri::command]
pub(crate) fn global_search(
    state: State<AppState>,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<search::GlobalHit>> {
    let limit = limit.unwrap_or(30).clamp(1, 100);
    search::global_search(&state, &query, limit)
}
