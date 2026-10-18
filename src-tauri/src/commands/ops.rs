use crate::{db, importer, index, ops, paths};
use crate::error::{AppError, Result};
use crate::state::{AppState};
use super::{with_db, emit_fs_changed, emit_projects_changed, emit_notes_changed};
use std::path::PathBuf;
use tauri::{AppHandle, State};

// One .zip with a consistent DB snapshot + Projects/ copy. Blocking zip work
// runs on the blocking pool with its own DB connection (same pattern as the
// embed-model download).
#[tauri::command]
pub(crate) async fn create_backup(state: State<'_, AppState>, dest: Option<String>) -> Result<ops::BackupInfo> {
    let wd = state.working_dir()?;
    let dest = dest.map(PathBuf::from);
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db::open(&paths::db_path(&wd))?;
        ops::create_backup(&wd, &conn, dest.as_deref())
    })
    .await
    .map_err(|e| AppError::Io(format!("backup task: {e}")))?
}

#[tauri::command]
pub(crate) fn clear_tmp(state: State<AppState>) -> Result<ops::TmpStats> {
    ops::clear_tmp(&state.working_dir()?)
}

#[tauri::command]
pub(crate) fn backups_dir(state: State<AppState>) -> Result<String> {
    let dir = paths::backups_dir(&state.working_dir()?);
    std::fs::create_dir_all(&dir)?;
    Ok(dir.display().to_string())
}

// Auto-backup state for the settings UI (SPEC 3.6): last backup time, the
// configured interval/keep, how many archives are on disk, and whether one is
// due now.
#[tauri::command]
pub(crate) fn backup_status(state: State<AppState>) -> Result<ops::BackupStatus> {
    ops::backup_status(&state.working_dir()?)
}

// Best-effort reindex + UI notify after a project/notes import.
pub(crate) fn notify_import(
    app: &AppHandle,
    state: &AppState,
    wd: &std::path::Path,
    target: &str,
    project_id: Option<i64>,
    dest_rel: &str,
) {
    match target {
        "project" => {
            if let Some(pid) = project_id {
                let _ = index::index_project(state, pid);
                emit_fs_changed(app, pid, dest_rel);
                emit_projects_changed(app);
            }
        }
        "notes" => {
            let _ = index::index_notes(wd);
            emit_notes_changed(app, dest_rel);
        }
        _ => {}
    }
}

#[tauri::command]
pub(crate) fn import_folder(
    app: AppHandle,
    state: State<AppState>,
    target: String,
    project_id: Option<i64>,
    source_path: String,
    dest_rel: Option<String>,
) -> Result<importer::ImportSummary> {
    let wd = state.working_dir()?;
    let dest_rel = dest_rel.unwrap_or_default();
    let summary = with_db(&state, |conn| {
        importer::import_folder(conn, &wd, &target, project_id, &source_path, &dest_rel)
    })?;
    notify_import(&app, &state, &wd, &target, project_id, &dest_rel);
    Ok(summary)
}

#[tauri::command]
pub(crate) fn import_archive(
    app: AppHandle,
    state: State<AppState>,
    target: String,
    project_id: Option<i64>,
    source_path: String,
    dest_rel: Option<String>,
) -> Result<importer::ImportSummary> {
    let wd = state.working_dir()?;
    let dest_rel = dest_rel.unwrap_or_default();
    let summary = with_db(&state, |conn| {
        importer::import_archive(conn, &wd, &target, project_id, &source_path, &dest_rel)
    })?;
    notify_import(&app, &state, &wd, &target, project_id, &dest_rel);
    Ok(summary)
}

#[tauri::command]
pub(crate) fn import_obsidian(
    app: AppHandle,
    state: State<AppState>,
    source_path: String,
    dest_rel: Option<String>,
) -> Result<importer::ImportSummary> {
    let wd = state.working_dir()?;
    let dest_rel = dest_rel.unwrap_or_default();
    let summary = importer::import_obsidian(&wd, &source_path, &dest_rel)?;
    let _ = index::index_notes(&wd);
    emit_notes_changed(&app, dest_rel);
    Ok(summary)
}

// Blocking zip work runs on the blocking pool with its own DB connection
// (same pattern as create_backup).
#[tauri::command]
pub(crate) async fn export_project(
    state: State<'_, AppState>,
    project_id: i64,
    dest_path: Option<String>,
) -> Result<importer::ExportSummary> {
    let wd = state.working_dir()?;
    let dest = dest_path.map(PathBuf::from);
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db::open(&paths::db_path(&wd))?;
        importer::export_project(&conn, &wd, project_id, dest.as_deref())
    })
    .await
    .map_err(|e| AppError::Io(format!("export task: {e}")))?
}

#[tauri::command]
pub(crate) async fn export_notes(
    state: State<'_, AppState>,
    dest_path: Option<String>,
) -> Result<importer::ExportSummary> {
    let wd = state.working_dir()?;
    let dest = dest_path.map(PathBuf::from);
    tauri::async_runtime::spawn_blocking(move || importer::export_notes(&wd, dest.as_deref()))
        .await
        .map_err(|e| AppError::Io(format!("export task: {e}")))?
}

// The OS plugin is the source of truth; a plugin/OS failure falls back to
// the app.json mirror instead of breaking the UI.
#[tauri::command]
pub(crate) fn get_autostart(app: AppHandle, state: State<AppState>) -> Result<bool> {
    use tauri_plugin_autostart::ManagerExt;
    match app.autolaunch().is_enabled() {
        Ok(enabled) => Ok(enabled),
        Err(_) => Ok(ops::autostart_choice(&state.working_dir()?)),
    }
}

#[tauri::command]
pub(crate) fn set_autostart(app: AppHandle, state: State<AppState>, enabled: bool) -> Result<bool> {
    use tauri_plugin_autostart::ManagerExt;
    let autolaunch = app.autolaunch();
    ops::apply_autostart(
        &|on| {
            if on {
                autolaunch.enable()
            } else {
                autolaunch.disable()
            }
            .map_err(|e| e.to_string())
        },
        enabled,
    )?;
    ops::set_autostart_choice(&state.working_dir()?, enabled)?;
    Ok(enabled)
}
