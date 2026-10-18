use crate::{fs_ops, open_with, reliability, undo};
use crate::error::{AppError, Result};
use crate::fs_ops::FsEntry;
use crate::state::{AppState};
use super::{with_db, emit_fs_changed, emit_trash_changed};
use tauri::{AppHandle, State};

#[tauri::command]
pub(crate) fn fs_list_dir(
    state: State<AppState>,
    project_id: i64,
    rel_path: String,
) -> Result<Vec<FsEntry>> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| fs_ops::list_dir(conn, &wd, project_id, &rel_path))
}

#[tauri::command]
pub(crate) fn fs_read_text(state: State<AppState>, project_id: i64, rel_path: String) -> Result<String> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| fs_ops::read_text(conn, &wd, project_id, &rel_path))
}

#[tauri::command]
pub(crate) fn fs_write_text(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    rel_path: String,
    content: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let previous = with_db(&state, |conn| {
        let path = fs_ops::resolve_existing(conn, &wd, project_id, &rel_path)?;
        let abs = path.display().to_string();
        let version_id = std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| reliability::version_save(conn, &wd, "project", &abs, &c).ok());
        Ok::<Option<(String, i64)>, AppError>(version_id.map(|id| (abs, id)))
    })
    .unwrap_or(None);
    let item = with_db(&state, |conn| {
        fs_ops::write_text(conn, &wd, project_id, &rel_path, &content)
    })?;
    if let Some((abs, version_id)) = previous {
        let _ = with_db(&state, |conn| {
            undo::push_file_write(
                conn,
                "project",
                &abs,
                Some(version_id),
                false,
                Some(project_id),
                Some(&item.rel_path),
            )
        });
    }
    emit_fs_changed(&app, project_id, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
pub(crate) fn fs_create_folder(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    parent_rel: String,
    name: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let item = with_db(&state, |conn| {
        fs_ops::create_folder(conn, &wd, project_id, &parent_rel, &name)
    })?;
    emit_fs_changed(&app, project_id, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
pub(crate) fn fs_create_file(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    parent_rel: String,
    name: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let item = with_db(&state, |conn| {
        fs_ops::create_file(conn, &wd, project_id, &parent_rel, &name)
    })?;
    emit_fs_changed(&app, project_id, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
pub(crate) fn fs_rename(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    rel_path: String,
    new_name: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let from_abs = with_db(&state, |conn| {
        fs_ops::resolve_existing(conn, &wd, project_id, &rel_path)
            .map(|p| p.display().to_string())
    })
    .ok();
    let item = with_db(&state, |conn| {
        fs_ops::rename(conn, &wd, project_id, &rel_path, &new_name)
    })?;
    if let Some(from_abs) = from_abs {
        let to_abs = with_db(&state, |conn| {
            fs_ops::resolve_existing(conn, &wd, project_id, &item.rel_path)
                .map(|p| p.display().to_string())
        })
        .ok();
        if let Some(to_abs) = to_abs {
            let _ = with_db(&state, |conn| {
                undo::push_rename(
                    conn,
                    "project",
                    &from_abs,
                    &to_abs,
                    Some(project_id),
                    Some(&item.rel_path),
                )
            });
        }
    }
    emit_fs_changed(&app, project_id, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
pub(crate) fn fs_copy_as(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    rel_path: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let item = with_db(&state, |conn| fs_ops::copy_as(conn, &wd, project_id, &rel_path))?;
    emit_fs_changed(&app, project_id, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
pub(crate) fn fs_absolute_path(state: State<AppState>, project_id: i64, rel_path: String) -> Result<String> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| fs_ops::absolute_path(conn, &wd, project_id, &rel_path))
}

#[tauri::command]
pub(crate) fn fs_reveal_in_finder(state: State<AppState>, project_id: i64, rel_path: String) -> Result<()> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| fs_ops::reveal_in_finder(conn, &wd, project_id, &rel_path))
}

#[tauri::command]
pub(crate) fn fs_extract_archive(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    rel_path: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let item = with_db(&state, |conn| fs_ops::extract_archive(conn, &wd, project_id, &rel_path))?;
    emit_fs_changed(&app, project_id, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
pub(crate) fn fs_delete(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    rel_path: String,
) -> Result<()> {
    let wd = state.working_dir()?;
    let path_abs = with_db(&state, |conn| {
        fs_ops::resolve_existing(conn, &wd, project_id, &rel_path)
            .map(|p| p.display().to_string())
    })
    .ok();
    // Resolve under a short DB read, move the item into the trash store
    // outside the DB mutex (a folder move can copy a whole tree), then the
    // single trash-row INSERT. Same order as before: move, then row.
    let (kind, path) = with_db(&state, |conn| {
        fs_ops::delete_plan(conn, &wd, project_id, &rel_path)
    })?;
    let staged = reliability::trash_stage(&wd, &path)?;
    let trash_id = with_db(&state, |conn| {
        reliability::trash_record(conn, "project", &kind, &path, &staged, None)
    })?;
    if let Some(path_abs) = path_abs {
        let _ = with_db(&state, |conn| {
            undo::push_delete(
                conn,
                trash_id,
                "project",
                &path_abs,
                Some(project_id),
                Some(&rel_path),
            )
        });
    }
    emit_fs_changed(&app, project_id, rel_path);
    emit_trash_changed(&app, Some(trash_id));
    Ok(())
}

#[tauri::command]
pub(crate) fn fs_move(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    from_rel: String,
    to_dir_rel: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let from_abs = with_db(&state, |conn| {
        fs_ops::resolve_existing(conn, &wd, project_id, &from_rel)
            .map(|p| p.display().to_string())
    })
    .ok();
    let item = with_db(&state, |conn| {
        fs_ops::move_entry(conn, &wd, project_id, &from_rel, &to_dir_rel)
    })?;
    if let Some(from_abs) = from_abs {
        let to_abs = with_db(&state, |conn| {
            fs_ops::resolve_existing(conn, &wd, project_id, &item.rel_path)
                .map(|p| p.display().to_string())
        })
        .ok();
        if let Some(to_abs) = to_abs {
            let _ = with_db(&state, |conn| {
                undo::push_rename(
                    conn,
                    "project",
                    &from_abs,
                    &to_abs,
                    Some(project_id),
                    Some(&item.rel_path),
                )
            });
        }
    }
    emit_fs_changed(&app, project_id, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
pub(crate) fn fs_import(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    dest_rel: String,
    source_paths: Vec<String>,
) -> Result<Vec<FsEntry>> {
    let wd = state.working_dir()?;
    let items = with_db(&state, |conn| {
        fs_ops::import(conn, &wd, project_id, &dest_rel, source_paths)
    })?;
    emit_fs_changed(&app, project_id, dest_rel);
    Ok(items)
}

#[tauri::command]
pub(crate) fn fs_open_external(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    rel_path: String,
) -> Result<()> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| {
        fs_ops::open_external(&app, conn, &wd, project_id, &rel_path)
    })
}

// "Open with" chooser: the LaunchServices handler apps for an extension
// (non-stock first), the remembered per-extension choice, and opening a file
// with a chosen app via `open -a`.
#[tauri::command]
pub(crate) fn list_open_with_apps(ext: String) -> Vec<open_with::AppInfo> {
    open_with::list_apps(&ext)
}

#[tauri::command]
pub(crate) fn get_open_with(state: State<AppState>, ext: String) -> Result<Option<open_with::AppInfo>> {
    open_with::get_remembered(&state.working_dir()?, &ext)
}

#[tauri::command]
pub(crate) fn remember_open_with(state: State<AppState>, ext: String, app_path: String) -> Result<()> {
    open_with::remember(&state.working_dir()?, &ext, &app_path)
}

#[tauri::command]
pub(crate) fn open_with(
    state: State<AppState>,
    scope: String,
    project_id: Option<i64>,
    path: String,
    app_path: Option<String>,
) -> Result<open_with::OpenResult> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| {
        open_with::open_with(conn, &wd, &scope, project_id, &path, app_path.as_deref())
    })
}

#[tauri::command]
pub(crate) fn fs_read_binary_data_url(
    state: State<AppState>,
    project_id: i64,
    rel_path: String,
    max_bytes: u64,
) -> Result<String> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| {
        fs_ops::read_binary_data_url(conn, &wd, project_id, &rel_path, max_bytes)
    })
}
