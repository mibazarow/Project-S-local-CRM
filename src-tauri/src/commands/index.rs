use crate::{chat, embed, index, search_reindex};
use crate::error::{AppError, Result};
use crate::state::{AppState};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

#[tauri::command]
pub(crate) fn index_notes(app: AppHandle, state: State<AppState>) -> Result<index::IndexStats> {
    let stats = index::index_notes(&state.working_dir()?)?;
    let _ = app.emit(
        "index-changed",
        index::IndexChanged {
            project_id: index::NOTES_SCOPE_ID,
            stats: stats.clone(),
        },
    );
    Ok(stats)
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectIndexStats {
    project_id: i64,
    stats: index::IndexStats,
}

#[tauri::command]
pub(crate) fn index_project(app: AppHandle, state: State<AppState>, project_id: i64) -> Result<index::IndexStats> {
    let stats = index::index_project(&state, project_id)?;
    let _ = app.emit(
        "index-changed",
        index::IndexChanged {
            project_id,
            stats: stats.clone(),
        },
    );
    Ok(stats)
}

fn emit_results(app: &AppHandle, results: Vec<(i64, index::IndexStats)>) -> Vec<ProjectIndexStats> {
    results
        .into_iter()
        .map(|(project_id, stats)| {
            let _ = app.emit(
                "index-changed",
                index::IndexChanged {
                    project_id,
                    stats: stats.clone(),
                },
            );
            ProjectIndexStats { project_id, stats }
        })
        .collect()
}

#[tauri::command]
pub(crate) fn reindex_all(app: AppHandle, state: State<AppState>) -> Result<Vec<ProjectIndexStats>> {
    Ok(emit_results(&app, index::index_all(&state)?))
}

// Hard rebuild: wipes the whole file index in one transaction and reindexes
// every project and the Notes vault from scratch; same "index-changed"
// progress events as reindex_all.
#[tauri::command]
pub(crate) fn reindex_hard(app: AppHandle, state: State<AppState>) -> Result<Vec<ProjectIndexStats>> {
    Ok(emit_results(&app, index::reindex_hard(&state)?))
}

#[tauri::command]
pub(crate) fn index_status(state: State<AppState>, project_id: i64) -> Result<index::IndexStatus> {
    index::index_status(&state, project_id)
}

// Search reindex («Переиндексация поиска»): manual start from the top bar;
// the pass runs on its own thread and reports via "reindex-progress" /
// "reindex-done". The same pass also fires automatically every 5 minutes
// (background tick started in setup / choose_working_dir).
#[tauri::command]
pub(crate) fn reindex_start(app: AppHandle, state: State<AppState>) -> Result<search_reindex::ReindexStatus> {
    let wd = state.working_dir()?;
    Ok(search_reindex::global().start(wd, Arc::new(app)))
}

#[tauri::command]
pub(crate) fn reindex_status() -> search_reindex::ReindexStatus {
    search_reindex::status()
}

#[tauri::command]
pub(crate) fn embed_status(state: State<AppState>) -> Result<embed::EmbedStatus> {
    let wd = state.working_dir()?;
    Ok(embed::status(&wd))
}

// Downloads the EmbeddingGemma 2 text encoder from Hugging Face (needs the
// ort-backend feature for TLS + sha256 verification) and returns the new
// embed status. Progress is emitted as "embed-model-progress" events.
#[tauri::command]
pub(crate) async fn download_embed_model(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<embed::EmbedStatus> {
    let wd = state.working_dir()?;
    let emitter = app.clone();
    let download_dir = wd.clone();
    tauri::async_runtime::spawn_blocking(move || {
        embed::model_store::download_text_model(&download_dir, |file, done, total| {
            let _ = emitter.emit(
                "embed-model-progress",
                serde_json::json!({ "file": file, "done": done, "total": total }),
            );
        })
    })
    .await
    .map_err(|e| AppError::Io(format!("embed model download task: {e}")))??;
    Ok(embed::status(&wd))
}

#[tauri::command]
pub(crate) fn index_chat_messages(state: State<AppState>) -> Result<usize> {
    chat::index_chat_messages(&state)
}
