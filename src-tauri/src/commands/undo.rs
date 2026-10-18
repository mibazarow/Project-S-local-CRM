use crate::{undo};
use crate::error::{Result};
use crate::state::{AppState};
use super::{with_db, emit_fs_changed, emit_tasks_changed, emit_trash_changed, emit_notes_changed};
use tauri::{AppHandle, State};

// Bounded undo stack (SPEC undo slice): undo_last pops and reverses the newest
// mutation, undo_list previews the stack. Undo is best-effort: a failed undo is
// reported to the caller, but the mutation that pushed the entry already ran.
#[tauri::command]
pub(crate) fn undo_last(app: AppHandle, state: State<AppState>) -> Result<undo::UndoOutcome> {
    let wd = state.working_dir()?;
    let out = with_db(&state, |conn| undo::undo_last(conn, &wd))?;
    if let Some(fs) = &out.fs_event {
        emit_fs_changed(&app, fs.project_id, fs.rel_path.clone());
    }
    if let Some(rel) = &out.notes_rel_path {
        emit_notes_changed(&app, rel.clone());
    }
    if let Some(project_id) = out.task_project_id {
        emit_tasks_changed(&app, project_id);
    }
    if out.trash_id.is_some() {
        emit_trash_changed(&app, out.trash_id);
    }
    Ok(out)
}

#[tauri::command]
pub(crate) fn undo_list(
    state: State<AppState>,
    limit: Option<usize>,
) -> Result<Vec<undo::UndoEntryDto>> {
    with_db(&state, |conn| undo::list(conn, limit))
}
