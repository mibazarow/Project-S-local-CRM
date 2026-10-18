use crate::{chat, fs_ops, notes, reliability, undo};
use crate::error::{Result};
use crate::fs_ops::FsEntry;
use crate::state::{AgentRegistry, AppState, RunKey};
use super::{with_db, emit_trash_changed, emit_versions_changed, emit_notes_changed};
use std::sync::Arc;
use tauri::{AppHandle, State};

// Notes vault (top-level Notes/ folder): browse, edit and search markdown
// notes. Paths are vault-relative; the watcher reindexes Notes/ debounced,
// mutations notify the UI with "notes-changed" (and "index-changed" with
// project_id = NOTES_SCOPE_ID once the reindex lands).
#[tauri::command]
pub(crate) fn notes_ensure(state: State<AppState>) -> Result<String> {
    let dir = notes::ensure(&state.working_dir()?)?;
    Ok(dir.display().to_string())
}

#[tauri::command]
pub(crate) fn notes_tree(state: State<AppState>) -> Result<Vec<FsEntry>> {
    notes::tree(&state.working_dir()?)
}

#[tauri::command]
pub(crate) fn notes_list(state: State<AppState>, rel: Option<String>) -> Result<Vec<FsEntry>> {
    notes::list(&state.working_dir()?, rel.as_deref().unwrap_or(""))
}

#[tauri::command]
pub(crate) fn notes_read(state: State<AppState>, rel: String) -> Result<String> {
    notes::read(&state.working_dir()?, &rel)
}

#[tauri::command]
pub(crate) fn notes_write(
    app: AppHandle,
    state: State<AppState>,
    rel: String,
    content: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    // Snapshot the previous content before an overwrite (version history);
    // the absolute vault path is the same key the notes_write tool uses.
    let previous = notes::vault_root(&wd)
        .ok()
        .and_then(|root| fs_ops::contain_existing(&root, &rel, "Notes").ok())
        .and_then(|p| std::fs::read_to_string(&p).ok().map(|c| (p.display().to_string(), c)));
    let item = notes::write(&wd, &rel, &content)?;
    let version_id = previous
        .as_ref()
        .and_then(|(abs, prev)| {
            let id = with_db(&state, |conn| {
                reliability::version_save(conn, &wd, "notes", abs, prev)
            })
            .ok();
            if id.is_some() {
                emit_versions_changed(&app, "notes", abs);
            }
            id
        });
    if let (Some((abs, _)), Some(version_id)) = (&previous, version_id) {
        let _ = with_db(&state, |conn| {
            undo::push_file_write(
                conn,
                "notes",
                abs,
                Some(version_id),
                false,
                None,
                Some(&item.rel_path),
            )
        });
    }
    emit_notes_changed(&app, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
pub(crate) fn notes_create(
    app: AppHandle,
    state: State<AppState>,
    rel: String,
    content: Option<String>,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let item = notes::create(&wd, &rel, content.as_deref())?;
    let created_abs = notes::vault_root(&wd)
        .ok()
        .and_then(|root| fs_ops::contain_existing(&root, &item.rel_path, "Notes").ok())
        .map(|p| p.display().to_string());
    if let Some(created_abs) = created_abs {
        let _ = with_db(&state, |conn| {
            undo::push_file_write(
                conn,
                "notes",
                &created_abs,
                None,
                true,
                None,
                Some(&item.rel_path),
            )
        });
    }
    emit_notes_changed(&app, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
pub(crate) fn notes_rename(
    app: AppHandle,
    state: State<AppState>,
    rel: String,
    new_name: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let from_abs = notes::vault_root(&wd)
        .ok()
        .and_then(|root| fs_ops::contain_existing(&root, &rel, "Notes").ok())
        .map(|p| p.display().to_string());
    let item = notes::rename(&wd, &rel, &new_name)?;
    if let Some(from_abs) = from_abs {
        let to_abs = notes::vault_root(&wd)
            .ok()
            .and_then(|root| fs_ops::contain_existing(&root, &item.rel_path, "Notes").ok())
            .map(|p| p.display().to_string());
        if let Some(to_abs) = to_abs {
            let _ = with_db(&state, |conn| {
                undo::push_rename(conn, "notes", &from_abs, &to_abs, None, Some(&item.rel_path))
            });
        }
    }
    emit_notes_changed(&app, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
pub(crate) fn notes_mkdir(
    app: AppHandle,
    state: State<AppState>,
    parent_rel: Option<String>,
    name: String,
) -> Result<FsEntry> {
    let item = notes::mkdir(
        &state.working_dir()?,
        parent_rel.as_deref().unwrap_or(""),
        &name,
    )?;
    emit_notes_changed(&app, item.rel_path.clone());
    Ok(item)
}

// Move never overwrites: a name clash in the target folder is an error.
#[tauri::command]
pub(crate) fn notes_move(
    app: AppHandle,
    state: State<AppState>,
    from_rel: String,
    to_dir_rel: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let from_abs = notes::vault_root(&wd)
        .ok()
        .and_then(|root| fs_ops::contain_existing(&root, &from_rel, "Notes").ok())
        .map(|p| p.display().to_string());
    let item = notes::move_entry(&wd, &from_rel, &to_dir_rel)?;
    if let Some(from_abs) = from_abs {
        let to_abs = notes::vault_root(&wd)
            .ok()
            .and_then(|root| fs_ops::contain_existing(&root, &item.rel_path, "Notes").ok())
            .map(|p| p.display().to_string());
        if let Some(to_abs) = to_abs {
            let _ = with_db(&state, |conn| {
                undo::push_note_move(conn, &from_abs, &to_abs, &item.rel_path)
            });
        }
    }
    emit_notes_changed(&app, item.rel_path.clone());
    Ok(item)
}

// Copy keeps the source and suffixes the new entry on a name clash.
#[tauri::command]
pub(crate) fn notes_copy(
    app: AppHandle,
    state: State<AppState>,
    from_rel: String,
    to_dir_rel: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let item = notes::copy(&wd, &from_rel, &to_dir_rel)?;
    let created_abs = notes::vault_root(&wd)
        .ok()
        .and_then(|root| fs_ops::contain_existing(&root, &item.rel_path, "Notes").ok())
        .map(|p| p.display().to_string());
    if let Some(created_abs) = created_abs {
        let _ = with_db(&state, |conn| {
            undo::push_note_copy(conn, &created_abs, &item.rel_path)
        });
    }
    emit_notes_changed(&app, item.rel_path.clone());
    Ok(item)
}

// Finder drag-and-drop (SPEC 4.2): copies files/folders from outside into the
// vault (source untouched, collisions uniquified, escapes refused). The tree
// refreshes through "notes-changed"; the watcher reindexes the copied files.
#[tauri::command]
pub(crate) fn notes_import(
    app: AppHandle,
    state: State<AppState>,
    dest_rel: String,
    source_paths: Vec<String>,
) -> Result<Vec<FsEntry>> {
    let items = notes::import(&state.working_dir()?, &dest_rel, source_paths)?;
    for item in &items {
        emit_notes_changed(&app, item.rel_path.clone());
    }
    Ok(items)
}

#[tauri::command]
pub(crate) fn notes_delete(app: AppHandle, state: State<AppState>, rel: String) -> Result<()> {
    let wd = state.working_dir()?;
    let path_abs = notes::vault_root(&wd)
        .ok()
        .and_then(|root| fs_ops::contain_existing(&root, &rel, "Notes").ok())
        .map(|p| p.display().to_string());
    let trash_id = with_db(&state, |conn| notes::delete(conn, &wd, &rel))?;
    if let Some(path_abs) = path_abs {
        let _ = with_db(&state, |conn| {
            undo::push_delete(conn, trash_id, "notes", &path_abs, None, Some(&rel))
        });
    }
    emit_notes_changed(&app, rel);
    emit_trash_changed(&app, Some(trash_id));
    Ok(())
}

// Notes chat (SPEC notes chat slice): one continuous chat per working dir.
// Get-or-creates the singleton and returns its chat id.
#[tauri::command]
pub(crate) fn ensure_notes_chat(state: State<AppState>) -> Result<i64> {
    with_db(&state, notes::ensure_notes_chat)
}

#[tauri::command]
pub(crate) fn notes_chat_history(state: State<AppState>) -> Result<Vec<chat::MessageDto>> {
    with_db(&state, notes::chat_history)
}

// Persists the user turn into the notes chat and starts the notes agent on a
// background thread; progress arrives as "agent-event" /
// "agent-confirmation" events carrying { chatId, notes: true }. Returns the
// notes chat id.
#[tauri::command]
pub(crate) fn send_notes_message(
    app: AppHandle,
    state: State<AppState>,
    registry: State<'_, Arc<AgentRegistry>>,
    text: String,
) -> Result<i64> {
    notes::run_notes_chat(&app, &state, registry.inner(), &text)
}

#[tauri::command]
pub(crate) fn cancel_notes_run(registry: State<'_, Arc<AgentRegistry>>) -> Result<bool> {
    Ok(registry.request_cancel(RunKey::Notes))
}
