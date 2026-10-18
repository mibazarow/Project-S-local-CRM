use crate::{export, templates, watcher};
use crate::error::{Result};
use crate::fs_ops::FsEntry;
use crate::projects::ProjectDto;
use crate::tasks::TaskDto;
use crate::state::{AppState};
use super::{with_db, emit_fs_changed, emit_tasks_changed, emit_projects_changed, emit_notes_changed};
use tauri::{AppHandle, State};

// Reusable templates (Settings/config/templates.json, seeded on first use):
// pickers list name + short description; the create commands instantiate one
// template as a project / task / note. template_document only returns the
// markdown, which the agent or UI fills in and creates via create_document.
#[tauri::command]
pub(crate) fn templates_list(state: State<AppState>) -> Result<templates::TemplatesSummary> {
    templates::summary(&state.working_dir()?)
}

#[tauri::command]
pub(crate) fn template_create_project(
    app: AppHandle,
    state: State<AppState>,
    template_name: String,
    project_name: Option<String>,
) -> Result<ProjectDto> {
    let wd = state.working_dir()?;
    let dto = with_db(&state, |conn| {
        templates::create_project(conn, &wd, &template_name, project_name.as_deref())
    })?;
    watcher::start(&app, &state, &wd);
    emit_fs_changed(&app, dto.id, "");
    emit_projects_changed(&app);
    Ok(dto)
}

#[tauri::command]
pub(crate) fn template_create_task(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    template_name: String,
) -> Result<TaskDto> {
    let wd = state.working_dir()?;
    let dto = with_db(&state, |conn| {
        let dto = templates::create_task(conn, &wd, project_id, &template_name)?;
        export::export_project(conn, &wd, project_id)?;
        Ok(dto)
    })?;
    emit_tasks_changed(&app, dto.project_id);
    Ok(dto)
}

#[tauri::command]
pub(crate) fn template_create_note(
    app: AppHandle,
    state: State<AppState>,
    template_name: String,
    note_name: Option<String>,
) -> Result<FsEntry> {
    let item = templates::create_note(
        &state.working_dir()?,
        &template_name,
        note_name.as_deref(),
    )?;
    emit_notes_changed(&app, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
pub(crate) fn template_document(state: State<AppState>, template_name: String) -> Result<templates::DocumentDto> {
    templates::document(&state.working_dir()?, &template_name)
}
