use crate::{export, index, notes, projects, tasks};
use crate::error::{AppError, Result};
use crate::state::{AppState};
use super::{with_db, emit_tasks_changed, emit_notes_changed};
use tauri::{AppHandle, State};

// Quick capture (SPEC quick capture slice): the capture modal (global
// shortcut / tray menu) persists one entry either as a timestamped bullet in
// the daily Notes/Входящие note or as a new task in the chosen project
// (falling back to the first project when none is selected).
#[derive(Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CaptureTarget {
    Note,
    Task,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QuickCaptureResult {
    kind: &'static str,
    path: Option<String>,
    task_id: Option<i64>,
}

#[tauri::command]
pub(crate) fn quick_capture(
    app: AppHandle,
    state: State<AppState>,
    text: String,
    target: CaptureTarget,
    project_id: Option<i64>,
) -> Result<QuickCaptureResult> {
    let text = text.trim();
    if text.is_empty() {
        return Err(AppError::Invalid("capture text is empty".into()));
    }
    let wd = state.working_dir()?;
    match target {
        CaptureTarget::Note => {
            let rel = notes::append_inbox(&wd, text)?;
            // Best-effort: make the new bullet searchable right away.
            let _ = index::index_notes(&wd);
            emit_notes_changed(&app, rel.clone());
            Ok(QuickCaptureResult {
                kind: "note",
                path: Some(rel),
                task_id: None,
            })
        }
        CaptureTarget::Task => {
            let title: String = text.chars().take(200).collect();
            let dto = with_db(&state, |conn| {
                let pid = match project_id {
                    Some(id) => id,
                    None => projects::first_project_id(conn)?.ok_or_else(|| {
                        AppError::Invalid("no projects: create a project first".into())
                    })?,
                };
                let dto = tasks::task_create_with(
                    conn,
                    pid,
                    &title,
                    Some("new".into()),
                    None,
                    None,
                    None,
                    None,
                )?;
                export::export_project(conn, &wd, pid)?;
                Ok(dto)
            })?;
            emit_tasks_changed(&app, dto.project_id);
            Ok(QuickCaptureResult {
                kind: "task",
                path: None,
                task_id: Some(dto.id),
            })
        }
    }
}
